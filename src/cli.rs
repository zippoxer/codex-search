use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Parser};

use crate::DEFAULT_LIMIT;
use crate::discovery::{self, DiscoveryOptions};
use crate::search::search_sessions;
use crate::session::{SearchResult, Session};
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
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let query = args.query.join(" ").trim().to_owned();

    let mut discovery = DiscoveryOptions::with_defaults()?;
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

    if !wants_tui || !is_tty {
        if wants_tui && !is_tty {
            eprintln!(
                "Interactive TUI disabled: standard streams are not attached to a TTY. Falling back to list output."
            );
        }
        let sessions = if root_exists {
            discovery::collect_sessions(&discovery)?
        } else {
            Vec::new()
        };
        run_cli_mode(
            &sessions,
            &query,
            args.limit,
            args.json,
            &discovery.root,
            root_exists,
        )?;
        return Ok(());
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
) -> Result<()> {
    if sessions.is_empty() {
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

    let results = search_sessions(sessions, query, limit)?;
    if json {
        serde_json::to_writer_pretty(std::io::stdout(), &results)
            .context("failed to serialize results")?;
        println!();
        return Ok(());
    }

    let now = OffsetDateTime::now_utc();
    for SearchResult {
        session,
        matched_message,
        snippet,
        ..
    } in results
    {
        let updated = format_timestamp(session.updated_at);
        let relative = format_relative(session.updated_at, now);
        let msg_time = session
            .latest_message_time
            .map(|dt| format_time_of_day(dt))
            .unwrap_or_else(|| "--:--".into());
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
        let snippet_line = snippet_to_cli_line(&snippet);
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

use time::OffsetDateTime;
