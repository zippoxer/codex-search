use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::SystemTime;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, unbounded};
use directories::BaseDirs;
use once_cell::sync::Lazy;
use rayon::prelude::*;
use regex::Regex;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};
use walkdir::WalkDir;

use crate::session::{Message, MessageRole, Session};

const SEARCH_BLOB_LIMIT: usize = 64 * 1024;
const MAX_MESSAGE_CHARS: usize = 8 * 1024;

pub struct SessionStream {
    receiver: Receiver<Session>,
    handle: thread::JoinHandle<()>,
    pub total: usize,
}

impl SessionStream {
    pub fn receiver(&self) -> Receiver<Session> {
        self.receiver.clone()
    }

    pub fn join(self) {
        let _ = self.handle.join();
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub root: PathBuf,
    pub scan_limit: usize,
    pub preview_char_limit: usize,
}

impl DiscoveryOptions {
    pub fn with_defaults() -> Result<Self> {
        Ok(Self {
            root: default_sessions_dir()?,
            // Lower default scan limit to keep TUI snappy on large datasets
            scan_limit: 50,
            preview_char_limit: 240,
        })
    }
}

pub fn default_sessions_dir() -> Result<PathBuf> {
    let base = BaseDirs::new().context("failed to determine home directory")?;
    Ok(base.home_dir().join(".codex/sessions"))
}

pub fn collect_sessions(options: &DiscoveryOptions) -> Result<Vec<Session>> {
    let paths = collect_session_paths(options)?;
    let sessions: Vec<Session> = paths
        .into_par_iter()
        .filter_map(|path| {
            load_session_from_path(path, options.preview_char_limit)
                .ok()
                .flatten()
        })
        .collect();
    Ok(sessions)
}

pub fn collect_session_paths(options: &DiscoveryOptions) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<(PathBuf, OffsetDateTime)> = WalkDir::new(&options.root)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => {
                if entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                {
                    let modified = entry
                        .metadata()
                        .ok()
                        .and_then(|meta| meta.modified().ok())
                        .map(system_time_to_offset);
                    modified.map(|ts| (entry.into_path(), ts))
                } else {
                    None
                }
            }
            Err(_) => None,
        })
        .collect();

    entries.sort_by(|a, b| b.1.cmp(&a.1));

    Ok(entries
        .into_iter()
        .map(|(path, _)| path)
        .take(options.scan_limit)
        .collect())
}

pub fn stream_sessions(paths: Vec<PathBuf>, preview_char_limit: usize) -> SessionStream {
    let total = paths.len();
    let (tx, rx) = unbounded();

    let handle = thread::spawn(move || {
        for path in paths {
            let display = path.clone();
            match load_session_from_path(path, preview_char_limit) {
                Ok(Some(session)) => {
                    if tx.send(session).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("failed to load session {:?}: {err}", display);
                }
            }
        }
    });

    SessionStream {
        receiver: rx,
        handle,
        total,
    }
}

pub fn load_session_from_path(path: PathBuf, preview_char_limit: usize) -> Result<Option<Session>> {
    let metadata = std::fs::metadata(&path).context("reading session metadata")?;
    let updated_at = system_time_to_offset(metadata.modified()?);

    let file = File::open(&path).with_context(|| format!("opening session {:?}", path))?;
    let reader = BufReader::new(file);

    let mut messages: Vec<Message> = Vec::new();
    let mut search_blob = String::new();
    let mut detected_cwd: Option<PathBuf> = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some((msg, full_text, is_meta)) = extract_message(&value, preview_char_limit) {
                if !is_meta {
                    if search_blob.len() + full_text.len() + 1 < SEARCH_BLOB_LIMIT {
                        if !search_blob.is_empty() {
                            search_blob.push('\n');
                        }
                        search_blob.push_str(&full_text);
                    }
                    messages.push(msg);
                }
                // Always try to detect cwd regardless of meta flag; capture only once
                if detected_cwd.is_none() {
                    if let Some(cwd) = extract_cwd_from_text(&full_text) {
                        detected_cwd = Some(cwd);
                    }
                }
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let (label, created_at, uuid) = parse_from_filename(&path)?;
    let label_lower = label.to_lowercase();
    let uuid_lower = uuid.to_lowercase();
    let latest_message_time = messages.iter().filter_map(|m| m.timestamp).max();

    if !search_blob.is_empty() {
        search_blob.push('\n');
    }
    search_blob.push_str(&label);
    search_blob.push('\n');
    search_blob.push_str(&uuid);
    let search_blob_lower = search_blob.to_lowercase();
    let search_blob_ws_lower = collapse_ws_lower(&search_blob_lower);

    Ok(Some(Session {
        uuid,
        label,
        label_lower,
        path,
        created_at,
        updated_at,
        latest_message_time,
        cwd: detected_cwd,
        messages,
        search_blob,
        search_blob_lower,
        search_blob_ws_lower,
        uuid_lower,
    }))
}

fn extract_message(value: &Value, preview_char_limit: usize) -> Option<(Message, String, bool)> {
    // Supported shapes:
    // 1) { type: "response_item", payload: { type: "message", role: "user"|"assistant", content: [...] } }
    // 2) { type: "event_msg", payload: { type: "user_message", message: "..." } }
    // 3) Flat: { role: "user"|"assistant", content: [...] }
    // Special-case: event stream user_message (no content array)
    if let Some(payload) = value.get("payload") {
        let payload_obj = payload.as_object()?;
        match payload_obj.get("type").and_then(Value::as_str) {
            Some("user_message") => {
                let content_text = payload_obj.get("message").and_then(Value::as_str)?;
                let timestamp = payload_obj
                    .get("timestamp")
                    .or_else(|| payload_obj.get("create_time"))
                    .or_else(|| payload_obj.get("createTime"));

                let full_text = content_text.to_owned();
                let clipped = clip_chars(&full_text, MAX_MESSAGE_CHARS);
                let preview = make_preview(&clipped, preview_char_limit);
                let timestamp = timestamp
                    .and_then(parse_timestamp_value)
                    .or_else(|| extract_timestamp(value));
                let is_meta = is_meta_text(&full_text);
                return Some((
                    Message {
                        role: MessageRole::User,
                        text: preview,
                        timestamp,
                        full_text: clipped.clone(),
                        full_text_lower: clipped.to_lowercase(),
                        full_text_ws_lower: collapse_ws_lower(&clipped.to_lowercase()),
                    },
                    clipped,
                    is_meta,
                ));
            }
            Some("message") => {
                // Fall through to the generic path below
            }
            _ => return None,
        }
    }

    let (role_raw, content, ts_value) = if let Some(payload) = value.get("payload") {
        let payload_obj = payload.as_object()?;
        let role = payload_obj.get("role").and_then(Value::as_str)?;
        let content = payload_obj.get("content")?;
        let timestamp = payload_obj
            .get("timestamp")
            .or_else(|| payload_obj.get("create_time"))
            .or_else(|| payload_obj.get("createTime"));
        (role, content, timestamp)
    } else {
        let role = value.get("role").and_then(Value::as_str)?;
        let content = value.get("content")?;
        let timestamp = value
            .get("timestamp")
            .or_else(|| value.get("create_time"))
            .or_else(|| value.get("createTime"));
        (role, content, timestamp)
    };

    let role = match role_raw {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => return None,
    };

    let full_text = extract_text(content)?;
    let clipped = clip_chars(&full_text, MAX_MESSAGE_CHARS);
    let preview = make_preview(&clipped, preview_char_limit);

    let timestamp = ts_value
        .and_then(parse_timestamp_value)
        .or_else(|| extract_timestamp(value));

    let is_meta = is_meta_text(&full_text);

    Some((
        Message {
            role,
            text: preview,
            timestamp,
            full_text: clipped.clone(),
            full_text_lower: clipped.to_lowercase(),
            full_text_ws_lower: collapse_ws_lower(&clipped.to_lowercase()),
        },
        clipped,
        is_meta,
    ))
}

fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut acc = String::new();
            for item in items {
                if let Value::Object(map) = item {
                    // Only include visible user/assistant text; skip thinking/tool internals.
                    let allowed = match map.get("type").and_then(Value::as_str) {
                        Some(t) => matches!(
                            t,
                            "input_text"
                                | "output_text"
                                | "assistant_text"
                                | "text"
                        ),
                        None => true,
                    };
                    if allowed {
                        if let Some(Value::String(text)) = map.get("text") {
                        if !acc.is_empty() {
                            acc.push('\n');
                        }
                        acc.push_str(text);
                        }
                    }
                }
            }
            if acc.is_empty() { None } else { Some(acc) }
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(|s| s.to_owned()),
        _ => None,
    }
}

fn extract_timestamp(value: &Value) -> Option<OffsetDateTime> {
    if let Some(payload) = value.get("payload") {
        if let Some(ts) = extract_timestamp(payload) {
            return Some(ts);
        }
    }

    value
        .get("create_time")
        .or_else(|| value.get("createTime"))
        .or_else(|| value.get("timestamp"))
        .or_else(|| value.get("created_at"))
        .or_else(|| value.get("createdAt"))
        .and_then(parse_timestamp_value)
}

fn parse_timestamp_value(value: &Value) -> Option<OffsetDateTime> {
    if let Some(s) = value.as_str() {
        parse_datetime_string(s).ok()
    } else if let Some(n) = value.as_i64() {
        OffsetDateTime::from_unix_timestamp(n).ok()
    } else if let Some(n) = value.as_f64() {
        let nanos = (n * 1_000_000_000_f64).round() as i128;
        OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
    } else {
        None
    }
}

fn is_meta_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    const META_MARKERS: &[&str] = &[
        "<user_instructions>",
        "<environment_context>",
        "<system_instructions>",
        "<developer_instructions>",
        "<assistant_memory>",
        "<user_action>",
    ];
    META_MARKERS
        .iter()
        .any(|marker| trimmed.starts_with(marker))
}

fn make_preview(full_text: &str, limit: usize) -> String {
    let trimmed = full_text.trim();
    if limit == 0 {
        return String::new();
    }

    let mut char_iter = trimmed.chars();
    let take_count = limit.saturating_sub(1);
    let mut preview: String = char_iter.by_ref().take(take_count).collect();

    if char_iter.next().is_some() {
        preview.push('…');
        preview
    } else if preview.is_empty() {
        trimmed.to_owned()
    } else {
        preview
    }
}

fn extract_cwd_from_text(text: &str) -> Option<PathBuf> {
    static CWD_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?si)<environment_context>.*?<cwd>(?P<cwd>[^<]+)</cwd>.*?</environment_context>")
            .expect("invalid cwd regex")
    });
    let trimmed = text.trim();
    if let Some(caps) = CWD_RE.captures(trimmed) {
        let raw = caps.name("cwd").map(|m| m.as_str().trim().to_string())?;
        let expanded = expand_tilde(&raw);
        return Some(PathBuf::from(expanded));
    }
    None
}

fn expand_tilde(p: &str) -> String {
    if p.starts_with("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
            let mut s = home.to_string_lossy().to_string();
            if !s.ends_with('/') { s.push('/'); }
            s.push_str(&p[2..]);
            return s;
        }
    }
    p.to_string()
}

fn clip_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit { return text.to_owned(); }
    let mut it = text.chars();
    let mut out = String::with_capacity(limit + 1);
    for _ in 0..limit { if let Some(ch) = it.next() { out.push(ch); } else { break; } }
    out.push('…');
    out
}

fn collapse_ws_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn parse_from_filename(path: &Path) -> Result<(String, Option<OffsetDateTime>, String)> {
    static SESSION_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"^(?P<label>.+?)-(?P<datetime>\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2})-(?P<uuid>[0-9a-fA-F-]+)$",
        )
        .expect("invalid session regex")
    });

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid utf-8 in session filename")?;

    if let Some(caps) = SESSION_RE.captures(stem) {
        let label = caps
            .name("label")
            .map(|m| m.as_str().replace('-', " "))
            .unwrap_or_else(|| "session".into());
        let datetime = caps.name("datetime").map(|m| m.as_str());
        let uuid = caps
            .name("uuid")
            .map(|m| m.as_str().to_owned())
            .unwrap_or_else(|| stem.to_owned());

        let created_at = datetime.and_then(|s| parse_filename_datetime(s).ok());

        Ok((label, created_at, uuid))
    } else {
        Ok((stem.to_string(), None, stem.to_string()))
    }
}

fn parse_filename_datetime(raw: &str) -> Result<OffsetDateTime> {
    use time::macros::format_description;

    let (date, time_part) = raw
        .split_once('T')
        .context("missing 'T' separator in filename timestamp")?;
    let time_part = time_part.replace('-', ":");
    let candidate = format!("{date}T{time_part}");
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    let naive = PrimitiveDateTime::parse(&candidate, &format)?;
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    Ok(naive.assume_offset(offset))
}

fn parse_datetime_string(raw: &str) -> Result<OffsetDateTime> {
    let trimmed = raw.trim();
    if let Ok(dt) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Ok(dt);
    }

    let replaced = trimmed.replace(' ', "T");
    if let Ok(dt) = OffsetDateTime::parse(&replaced, &Rfc3339) {
        return Ok(dt);
    }

    parse_filename_datetime(&replaced)
}

fn system_time_to_offset(time: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(time)
}
