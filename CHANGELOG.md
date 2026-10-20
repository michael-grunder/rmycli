# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Initial project README describing the Redis client vision, metadata-based
  completion, bounded key discovery, and planned reply-size guidance.
- Shared contributor instructions in `AGENTS.md`, with `CLAUDE.md` linking
  to the same file.
- Standard Rust formatting configuration with an 80-column target.
- Clippy `all`, `nursery`, and `pedantic` warnings at the binary crate root,
  with formatting, strict linting, and tests required before every commit.
