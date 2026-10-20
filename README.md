# rmycli

A Rust Redis command-line client aiming to be to `redis-cli` what `mycli` is
to the MySQL client: a familiar interactive shell with intelligent completion
and useful guidance as you type.

## Status

This project is at the initial scaffold and design stage. The executable
currently prints `Hello, world!`. Redis connections, command execution,
completion, and reply-size hints described below are planned features.

## Project goals

- Ultimately support every command advertised by the connected server's
  `COMMAND` metadata, including subcommands and module commands.
- Complete command names, known argument tokens, and keys in context.
- Keep the interactive prompt responsive while discovering keys.
- Help users recognize potentially huge replies before executing a command.

Command execution should accept arbitrary commands without requiring a
handwritten implementation for each one. Completion and reply estimates may
be richer for some commands than others; missing metadata must not prevent
users from entering commands themselves. Full `redis-cli` option parity is
not an initial milestone.

## Intelligent completion

Discover the server's command catalog using [COMMAND][command]. Use
[COMMAND DOCS][command-docs], when available, for argument structure, literal
tokens, optional arguments, and alternatives. Use [key specifications][keys]
to identify key arguments, including commands with variable key positions.
Older metadata and unavailable or ACL-restricted introspection need graceful
fallbacks. Cache metadata per server and refresh it after reconnecting.

Key positions do not universally describe the Redis value type a command
expects. Keep explicit, tested type hints where needed, such as the list key
accepted by `LRANGE`. When the type is unknown, allow unfiltered key
completion. Destination keys may not exist yet; suggestions must never make
existing keys the only valid input.

For example, typing:

```text
lrange list:<Tab>
```

should begin cursor-based discovery with:

```text
SCAN 0 MATCH list:* TYPE list
```

Continue with the returned cursor as needed. The intended interaction is:

- No matches: leave the input unchanged.
- One match after discovery completes: insert the completed key.
- A few matches: open a popup; Tab and Shift-Tab cycle through candidates,
  Enter accepts a candidate without executing the command, and Escape closes
  the popup.
- More than a configurable threshold: ask before displaying the candidates.
  Show an exact count only after discovery finishes. Otherwise say "at least
  N matches" and ask before continuing discovery or displaying more results.

Use [SCAN][scan], never `KEYS`, for interactive key discovery. Deduplicate
results and continue past empty batches until the cursor reaches zero or a
budget is reached. `COUNT` is a work hint, not a result limit. Bound scan work,
memory, and elapsed time independently; support cancellation and discard
stale results when input changes. A partial scan must not claim uniqueness.

Treat the typed key prefix literally by escaping Redis glob characters
before appending `*`. Preserve key bytes, case, quoting, and escaping when
inserting a candidate. Scope discovery to the active server and database.
If `TYPE` filtering is unavailable, degrade gracefully. Cluster-wide
discovery needs an explicit design; a scan of one node is not a full catalog.

## Reply-size guidance

A later milestone will estimate potentially large replies before sending
the user's command. For example:

```text
lrange hugelist 0 -1
```

can use [LLEN][llen] to estimate the number of returned elements without
fetching them. If the estimate exceeds a configurable threshold, show a
confirmation such as:

```text
This command may return about 3,000,000 elements. Continue? [y/N]
```

For other `LRANGE` ranges, account for inclusive endpoints, negative indexes,
and bounds against the observed list length. Element count is not a byte
estimate, and the data may change between the estimate and execution.

Build estimators separately from completion, using cheap read-only probes.
Do not execute the requested command to estimate its reply. Unknown sizes
must remain unknown, not be reported as zero or safe. Estimator failures
should leave manual execution available. Confirmation must occur before the
original command is sent; declining leaves it unexecuted.

Interactive prompts, noninteractive behavior, and explicit overrides need
defined policies before this feature ships. Auxiliary requests must not be
injected into a user's transaction or other stateful command sequence.

## Implementation direction

Keep transport and session state, command metadata, input parsing,
completion, terminal presentation, and reply estimation separate. Prefer
testable logic independent of a live terminal or server. Choose libraries
as these components are implemented.

Planned milestones:

1. Establish a Redis connection and basic command execution loop.
2. Discover command metadata and complete commands and argument tokens.
3. Add key-position detection, bounded key discovery, and popup interaction.
4. Add reply-size estimation and confirmation, starting with `LRANGE`.
5. Expand command coverage and document server/version compatibility.

## Development

Use Rust with edition 2024 support, rustfmt, and Clippy installed.

```sh
cargo run
```

Use standard Rust formatting with an 80-column target, configured in
`rustfmt.toml`. Enable these lint groups at every crate root:

```rust
#![warn(clippy::all, clippy::nursery, clippy::pedantic)]
```

Before every commit, including documentation-only changes, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Fix failures before committing. Keep `Cargo.lock` under version control for
this application. See [AGENTS.md](AGENTS.md) for contributor instructions and
[CHANGELOG.md](CHANGELOG.md) for changes following Keep a Changelog.

[command]: https://redis.io/docs/latest/commands/command/
[command-docs]: https://redis.io/docs/latest/commands/command-docs/
[keys]: https://redis.io/docs/latest/develop/reference/key-specs/
[scan]: https://redis.io/docs/latest/commands/scan/
[llen]: https://redis.io/docs/latest/commands/llen/
