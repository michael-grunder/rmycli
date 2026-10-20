# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Working Redis shell with TCP, Unix socket, authenticated and verified TLS
  connections; command-line and stdin execution; and binary-safe reply output.
- Live syntax, summary, argument, version, group, and complexity help from
  `COMMAND DOCS`, with the full Redis documentation catalog bundled as a
  labeled fallback. Command and subcommand completion uses the live catalog.
- Metadata-driven key argument completion, separate Redis value-type hints,
  and a cancellable SCAN worker with literal prefix matching, deduplication,
  configurable discovery budgets, partial-result notices, and display prompts.
- Completion popup with Tab/Shift-Tab cycling and separate acceptance and
  execution; cursor editing, in-memory history, paste handling, and full help.
- `LRANGE` element-count estimates and confirmation, including a defined
  noninteractive refusal policy and explicit `--yes` override.
- Separate auxiliary connections, database and transaction state tracking,
  explicit reconnect with refreshed metadata, and connection cancellation.
- Pub/Sub and MONITOR reply display; stop interactive streams with Ctrl-C
  and reconnect before sending further commands.
- Isolated Redis integration fixtures and real-terminal smoke tests covering
  execution, completion, reply confirmation, cancellation, and streaming.
- Initial project README describing the Redis client vision, metadata-based
  completion, bounded key discovery, and planned reply-size guidance.
- Shared contributor instructions in `AGENTS.md`, with `CLAUDE.md` linking
  to the same file.
- Standard Rust formatting configuration with an 80-column target.
- Clippy `all`, `nursery`, and `pedantic` warnings at the binary crate root,
  with formatting, strict linting, and tests required before every commit.
