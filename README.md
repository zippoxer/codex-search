# Codex Search (Rust)

A fast terminal/TUI tool for searching and resuming Codex conversation history.

## Install

```bash
# install the release binary and expose both `cdxs` and `codex-search`
cargo install --git https://github.com/zippoxer/codex-search.git --force
```

## Usage

```bash
cdxs                 # launch streaming TUI (alias: codex-search)
cdxs sprite          # start with a query
cdxs --no-tui foo    # plain-text results (works without a TTY)
cdxs --json foo      # JSON output for scripting
```

Run `cdxs --help` (or `codex-search --help`) for all flags.

### Resume command

By default the TUI runs `codex --search resume {uuid}` when you press Enter.
Override via the `CODEX_SEARCH_RESUME` environment variable, e.g.:

```bash
export CODEX_SEARCH_RESUME="codex --search --dangerously-bypass-approvals-and-sandbox resume {uuid}"
```

You can also pass `--resume-command` on the CLI for one-off overrides.
