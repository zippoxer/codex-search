# Codex Search — Agents Guide

This document briefs engineers (human or AI) working on the Rust rewrite of the Codex session search tool located in `codex-search-rust/`.

## Project Overview

- **Purpose**: deliver a fast terminal workflow for locating Codex conversations and resuming them with minimal friction. Both a streaming TUI and non-interactive CLI outputs are supported.
- **Language**: Rust 1.76+ (edition 2024). `rustup` is installed if toolchain tweaks are necessary.
- **Performance goals**:
  - CLI responses under ~200 ms cold start.
  - TUI keystrokes echo immediately; matching runs off the UI thread.
  - Session discovery streams newest files first and must not freeze when >10 k JSONL files exist.

## Repository Layout

```
codex-search-rust/
├── Cargo.toml / Cargo.lock
├── src/
│   ├── cli.rs        # Clap-based argument parsing and mode orchestration
│   ├── discovery.rs  # Filesystem scanning, concurrent session loading
│   ├── search.rs     # Shared scoring utilities (Skim fuzzy matcher + recency)
│   ├── session.rs    # Data models for sessions/messages/results
│   ├── tui.rs        # ratatui UI, nucleo-powered live matcher
│   ├── util.rs       # Timestamp formatting helpers
│   ├── lib.rs / main.rs
└── AGENTS.md         # This guide
```

### Session Data

- Sessions live under `~/.codex/sessions` with nested `YYYY/MM/DD/` folders.
- Only user/assistant messages feed the searchable blob; instructions/tool calls are filtered.
- `Session.search_blob` is capped (~64 KB) to keep matching fast.

## Running

```bash
# Interactive TUI (requires a real tty)
cargo run --release --              # browse recent sessions

# Start with a query
cargo run --release -- sprite

# CLI fallbacks (usable inside restricted harnesses)
cargo run -- --no-tui "gold coin"
cargo run -- --list "shader"
cargo run -- --json "ItemEditor" | jq '.[0]'
```

### Key Flags

| Flag | Description |
|------|-------------|
| `--limit N` | Cap displayed results (default 20). |
| `--scan-limit N` | Limit filesystem scan depth (default 400 files). |
| `--sessions-dir PATH` | Override the Codex session directory (useful for tests). |
| `--resume-command CMD` | Shell template run when selecting a session (`{uuid}` placeholder). |
| `--dry-run` | Print the resume command instead of executing it. |
| `--no-tui` / `--list` / `--json` | Non-interactive modes. |

## Development Workflow

1. `cargo fmt`
2. `cargo check`
3. `cargo run -- --no-tui test`
4. `cargo run --release` (from a real terminal to validate streaming UI)

### Performance Checks

- Use `time cargo run -- --no-tui foo` for cold-start metrics.
- For TUI latency, temporarily log redraw timestamps (`RUST_LOG=debug`) and ensure the loop stays responsive (<16 ms echo).
- For large datasets, point `--sessions-dir` to synthetic data; the status bar should display `Indexing …` while results stream.

## Code Conventions

- Discovery streams sessions via channels; avoid blocking `Vec` scans in the TUI path.
- Sessions are wrapped in `Arc` when stored in UI state so nucleo can reference data lock-free.
- All ranking logic lives in `src/search.rs`; keep CLI and TUI behaviour consistent by using the shared `Scorer`.
- Time formatting in `src/util.rs` is intentionally terse (seconds/minutes/hours/days).
- Use `anyhow::Result` for user-facing fallbacks, and continue on malformed files—never panic during normal discovery.

## Dependencies

- Update `Cargo.toml` and regenerate `Cargo.lock` with `cargo generate-lockfile` (or let `cargo` update it).
- Favor lightweight crates. Current stack: ratatui, crossterm, crossbeam-channel, nucleo (TUI matcher), Skim matcher (CLI scoring).

## Common Tasks

### Adjusting Search Ranking

1. Tweak weights in `Scorer::score_session` / `best_message_for_session`.
2. `cargo run -- --no-tui <query>` to confirm order.
3. Inspect the TUI live to ensure responsiveness remains intact.

### Updating Session Parsing

1. Modify `extract_message` / `extract_text` in `src/discovery.rs` to handle new fields.
2. Keep `search_blob` bounded and relevant.
3. Re-run both CLI and TUI paths.

### TUI Changes

- Respect `query_dirty` / `results_dirty`; those flags drive incremental recomputation.
- Keep rendering allocation-free when possible.
- Add key bindings in `App::on_key` and remember to mark the query dirty when changes occur.

## Testing

- Currently manual. Add unit tests where logic can be isolated.
- Consider writing integration tests under `tests/` if behaviour becomes more complex.
- Always check both debug and release builds; release enables optimisations that affect responsiveness.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| TUI exits instantly with `fuzzy_optimal` panic | CLI path accidentally linked to nucleo matcher | Ensure `src/search.rs` uses Skim matcher; rebuild |
| “No sessions found” | Wrong directory or low scan limit | Use `--sessions-dir` or bump `--scan-limit` |
| TUI lags | Dirty flags not set or heavy work in draw loop | Audit `App::refresh_results` and avoid blocking operations |
| Resume command fails | Template missing `{uuid}` or command not in PATH | Use `--dry-run` to inspect output |

## Release / Distribution

- Build with `cargo build --release` and copy `target/release/codex-search-rust` into `$PATH`.
- Tool assumes Codex CLI sessions live at `~/.codex/sessions` unless overridden.

## Pre-Submit Checklist

- [ ] `cargo fmt`
- [ ] `cargo check`
- [ ] `cargo run -- --no-tui quick-test`
- [ ] `cargo run --release`
- [ ] Documentation updated (this file and CLI help) if behaviour changed

Stay disciplined about streaming design and performance checks to keep the search experience snappy.
