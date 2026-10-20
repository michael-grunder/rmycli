# Contributor instructions

These instructions apply throughout this repository. `CLAUDE.md` is a
relative symlink to this file; edit `AGENTS.md` to update shared guidance.

## Project intent

Build `rmycli`, a Rust client that brings a mycli-like interactive experience
to Redis. Read `README.md` for the design and current implementation status.
The long-term goal is support for every command described by the connected
server's `COMMAND` metadata, with intelligent argument and key completion
and eventual warnings before potentially huge replies.

Clearly distinguish implemented behavior from planned behavior in docs.

## Rust style and linting

- Follow standard Rust formatting with `max_width = 80` in `rustfmt.toml`.
  Keep prose near 80 columns where practical.
- Put the following at every Rust crate root, including future libraries:

  ```rust
  #![warn(clippy::all, clippy::nursery, clippy::pedantic)]
  ```

- Fix lint findings rather than disabling lint groups. Any necessary lint
  exception must be narrowly scoped and explain why it is appropriate.
- Keep dependencies justified and retain `Cargo.lock` for reproducible
  application builds.

## Required checks before every commit

Run these against the final changes, even for documentation-only commits:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Clippy is mandatory before committing any change. Do not bypass checks or
commit with failures. If a required tool or environment is unavailable,
report the blocker and leave the changes uncommitted.

Add meaningful tests for new behavior and bug fixes. For completion, cover
argument/key-position parsing, quoting, glob escaping, duplicate and empty
scan batches, cancellation, and threshold behavior. For reply estimation,
cover range bounds, unknown estimates, and declining confirmation. Use
isolated fixtures for integration tests that need Redis.

## Design constraints

- Derive command and argument knowledge from server metadata where possible.
  Keep command-specific type hints and reply estimators separate.
- Metadata gaps must not prevent users from submitting commands manually.
- Use bounded, cancellable `SCAN` discovery for keys, never `KEYS`.
  Do not present partial results as a complete count or a unique match.
- Preserve key bytes and command semantics; suggestions are optional.
- Keep network work from blocking terminal input. Separate completion
  results from command execution so accepting a candidate cannot execute it.
- Estimate reply sizes with cheap read-only requests before execution.
  Distinguish estimates from guarantees and element counts from bytes.
- Respect session state; background requests must not alter transaction or
  other stateful command behavior.

## Documentation and changelog

Update the README when behavior, setup, or design decisions change. Maintain
`CHANGELOG.md` using [Keep a Changelog](https://keepachangelog.com/en/1.1.0/):

- Record meaningful changes under `Unreleased`.
- Use `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, and `Security`
  sections as applicable; omit empty sections.
- Describe changes for users and contributors rather than copying commits.
- On release, move entries into a version heading dated `YYYY-MM-DD`,
  newest releases first. Keep an `Unreleased` section for subsequent work.
- Record delivered scaffolding as scaffolding, not as completed features.
