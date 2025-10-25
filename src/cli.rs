use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Parser};

use crate::DEFAULT_LIMIT;
use crate::discovery::{self, DiscoveryOptions};
use crate::search::search_sessions;
use crate::session::Session;
use crate::tui::{self, TuiConfig};
use crate::util::{format_relative, format_time_of_day, format_timestamp};

fn snippet_to_cli_line(snippet: &crate::session::Snippet) -> String {
    let mut out = String::new();
    for segment in &snippet.segments {
        if segment.highlighted {
            out.push_str("[1m");
            out.push_str(&segment.text);
            out.push_str("[0m");
        } else {
            out.push_str(&segment.text);
        }
    }
    out
}

#[derive(Debug, Parser)]
#[command(author, version, about = "Lightning fast Codex session search", long_about = None)]
pub struct Args {
    /// Search query terms (fuzzy matched against conversations)
    #[arg(trailing_var_arg = true)]
    pub query: Vec<String>,

    /// Number of results to return
    #[arg(short, long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,

    /// Print results as plain text list (disables TUI)
    #[arg(long, action = ArgAction::SetTrue)]
    pub list: bool,

    /// Emit results as JSON (disables TUI)
    #[arg(long, action = ArgAction::SetTrue)]
    pub json: bool,

    /// Disable the interactive TUI even without other output flags
    #[arg(long, action = ArgAction::SetTrue)]
    pub no_tui: bool,

    /// Restrict results to sessions tied to the current working directory (when available)
    #[arg(long, action = ArgAction::SetTrue)]
    pub cwd: bool,

    /// Maximum number of session files to scan
    #[arg(long)]
    pub scan_limit: Option<usize>,

    /// Override the sessions directory (defaults to ~/.codex/sessions)
    #[arg(long)]
    pub sessions_dir: Option<PathBuf>,

    /// Command template executed when a session is selected (use {uuid}).
    /// Defaults to the Codex CLI unless overridden by the CODEX_SEARCH_RESUME env var.
    #[arg(long, default_value = "")]
    pub resume_command: String,

    /// Preview character limit (affects in-memory message snippets)
    #[arg(long)]
    pub preview_limit: Option<usize>,

    /// Do not execute the resume command, just print it (useful for scripting)
    #[arg(long, action = ArgAction::SetTrue)]
    pub dry_run: bool,

    /// Run a headless benchmark and emit JSON metrics (no TUI)
    #[arg(long, action = ArgAction::SetTrue)]
    pub bench: bool,
    /// Benchmark iterations per query
    #[arg(long, default_value_t = 5)]
    pub bench_iters: usize,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let query = args.query.join(" ").trim().to_owned();

    let mut discovery = DiscoveryOptions::with_defaults()?;
    // Allow env override for scan limit; CLI flag still wins.
    if let Ok(val) = std::env::var("CODEX_SEARCH_SCAN_LIMIT") {
        if let Ok(n) = val.parse::<usize>() {
            discovery.scan_limit = n;
        }
    }
    if let Some(dir) = &args.sessions_dir {
        discovery.root = dir.clone();
    }
    if let Some(limit) = args.scan_limit {
        discovery.scan_limit = limit;
    }
    if let Some(preview) = args.preview_limit {
        discovery.preview_char_limit = preview;
    }

    let root_exists = discovery.root.exists();

    let wants_tui = !(args.json || args.list || args.no_tui);
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    if args.bench || !wants_tui || !is_tty {
        if wants_tui && !is_tty {
            eprintln!(
                "Interactive TUI disabled: standard streams are not attached to a TTY. Falling back to list output."
            );
        }
        // When filtering by CWD, widen the scan window so recent matches aren't missed.
        if args.cwd {
            discovery.scan_limit = discovery.scan_limit.max(1000);
        }
        let mut sessions = if root_exists {
            discovery::collect_sessions(&discovery)?
        } else {
            Vec::new()
        };
        if args.cwd {
            let cwd = std::env::current_dir().context("reading current directory")?;
            sessions = filter_sessions_by_cwd(sessions, &cwd);
        }
        if args.bench {
            return run_bench(&sessions, &query, args.bench_iters, args.limit, &discovery.root, root_exists);
        }
        // Keep a copy of cwd filter for potential auto-expand
        let cwd_opt = if args.cwd { Some(std::env::current_dir()?) } else { None };
        run_cli_mode(
            &sessions,
            &query,
            args.limit,
            args.json,
            &discovery.root,
            root_exists,
            cwd_opt.as_deref(),
        )?;
        return Ok(());
    }

    // TUI path: widen the scan window when filtering by CWD so older matching
    // sessions don’t disappear as the query gets more specific.
    if args.cwd {
        discovery.scan_limit = discovery.scan_limit.max(1000);
    }
    let session_paths = if root_exists {
        discovery::collect_session_paths(&discovery)?
    } else {
        Vec::new()
    };

    let empty_status = (!root_exists)
        .then(|| {
            format!(
                "Sessions directory {} does not exist",
                discovery.root.display()
            )
        })
        .or_else(|| {
            if session_paths.is_empty() {
                Some(format!(
                    "No Codex sessions found under {}",
                    discovery.root.display()
                ))
            } else {
                None
            }
        });

    let stream = discovery::stream_sessions(session_paths, discovery.preview_char_limit);
    let resume_template = if args.resume_command.is_empty() {
        std::env::var("CODEX_SEARCH_RESUME")
            .unwrap_or_else(|_| "codex --search resume {uuid}".to_string())
    } else {
        args.resume_command
    };

    tui::run(
        TuiConfig {
            limit: args.limit,
            resume_command: resume_template,
            dry_run: args.dry_run,
            initial_query: query,
            empty_status,
            total_expected: stream.total,
            filter_cwd: if args.cwd { Some(std::env::current_dir()?) } else { None },
        },
        stream,
    )
}

fn run_cli_mode(
    sessions: &[Session],
    query: &str,
    limit: usize,
    json: bool,
    sessions_root: &Path,
    root_exists: bool,
    cwd_filter: Option<&Path>,
) -> Result<()> {
    if sessions.is_empty() {
        // Try expanded scan before giving up when searching
        if !json && root_exists && !query.trim().is_empty() {
            let mut discovery = DiscoveryOptions::with_defaults()?;
            discovery.root = sessions_root.to_path_buf();
            discovery.scan_limit = 1000;
            let mut expanded = discovery::collect_sessions(&discovery)?;
            if let Some(cwd) = cwd_filter {
                expanded = filter_sessions_by_cwd(expanded, cwd);
            }
            if !expanded.is_empty() {
                return run_cli_mode(&expanded, query, limit, json, sessions_root, root_exists, cwd_filter);
            }
        }
        if json {
            println!("[]");
        } else {
            let hint = if root_exists {
                format!("no sessions discovered under {}", sessions_root.display())
            } else {
                format!(
                    "sessions directory {} does not exist",
                    sessions_root.display()
                )
            };
            eprintln!("{hint}");
        }
        return Ok(());
    }

    let mut results = search_sessions(sessions, query, limit)?;
    if !json && results.is_empty() && !query.trim().is_empty() && root_exists {
        // Auto-expand scan window and retry once
        let mut discovery = DiscoveryOptions::with_defaults()?;
        discovery.root = sessions_root.to_path_buf();
        discovery.scan_limit = 1000; // wider window for narrow queries
        let mut expanded = discovery::collect_sessions(&discovery)?;
        if let Some(cwd) = cwd_filter {
            expanded = filter_sessions_by_cwd(expanded, cwd);
        }
        results = search_sessions(&expanded, query, limit)?;
    }
    if json {
        serde_json::to_writer_pretty(std::io::stdout(), &results)
            .context("failed to serialize results")?;
        println!();
        return Ok(());
    }

    let now = OffsetDateTime::now_utc();
    for result in results {
        let session = &result.session;
        let matched_message = &result.matched_message;
        let snippet = &result.snippet;

        // Use the match anchor time (matched message -> latest message -> file mtime)
        let anchor = result.match_timestamp();
        let updated = format_timestamp(anchor);
        let relative = format_relative(anchor, now);
        let msg_time = format_time_of_day(anchor);
        let label = &session.label;
        let role = matched_message
            .as_ref()
            .map(|m| match m.role {
                crate::session::MessageRole::User => "you",
                crate::session::MessageRole::Assistant => "codex",
            })
            .unwrap_or("session");

        println!(
            "{uuid}\t{updated}\t{relative}\t{msg_time}\t{label} ({role})",
            uuid = session.uuid,
            updated = updated,
            relative = relative,
            msg_time = msg_time,
            label = label,
            role = role,
        );
        let snippet_line = snippet_to_cli_line(snippet);
        println!("    {}", snippet_line);
    }

    Ok(())
}

pub fn spawn_resume_command(command_template: &str, uuid: &str) -> Result<()> {
    let command = command_template.replace("{uuid}", uuid);
    let parts = shell_words::split(&command).context("failed to parse resume command")?;
    let program = parts
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("resume command is empty"))?;
    let args: Vec<String> = parts.into_iter().skip(1).collect();

    let status = Command::new(program).args(&args).status()?;
    if !status.success() {
        bail!("resume command failed with status {:?}", status.code());
    }
    Ok(())
}

fn filter_sessions_by_cwd(mut sessions: Vec<Session>, cwd: &Path) -> Vec<Session> {
    let cwd_norm = normalize_path(cwd);
    sessions
        .into_iter()
        .filter(|s| match &s.cwd {
            Some(p) => paths_related(&normalize_path(p), &cwd_norm),
            None => false,
        })
        .collect()
}

fn normalize_path(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn paths_related(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn run_bench(
    sessions: &[Session],
    query: &str,
    iters: usize,
    limit: usize,
    sessions_root: &Path,
    root_exists: bool,
) -> Result<()> {
    use std::time::Instant;
    let mut results = serde_json::json!({
        "root_exists": root_exists,
        "sessions_root": sessions_root.display().to_string(),
        "sessions_count": sessions.len(),
        "query": query,
        "limit": limit,
        "iterations": iters,
        "runs": [],
    });

    let mut runs = Vec::new();
    for _ in 0..iters {
        let t0 = Instant::now();
        let scored = search_sessions(sessions, query, limit)?;
        let t1 = t0.elapsed();
        runs.push(serde_json::json!({
            "search_ms": t1.as_millis(),
            "top_uuid": scored.get(0).map(|r| r.session.uuid.clone()),
        }));
    }
    results["runs"] = serde_json::Value::Array(runs);
    // Aggregate
    let times: Vec<u128> = results["runs"].as_array().unwrap().iter().filter_map(|v| v["search_ms"].as_u64().map(|x| x as u128)).collect();
    let avg = if times.is_empty() { 0.0 } else { (times.iter().sum::<u128>() as f64) / (times.len() as f64) };
    results["avg_search_ms"] = serde_json::json!(avg);

    serde_json::to_writer_pretty(std::io::stdout(), &results).context("failed to write bench json")?;
    println!();
    Ok(())
}

use time::OffsetDateTime;
