# rmycli

A Rust Redis shell with live command help, metadata-driven argument completion,
and type-aware key completion.

## Status

The first interactive implementation is available. It connects to Redis,
executes arbitrary commands, discovers the server's command catalog, shows
syntax and argument help while editing, and completes keys using bounded,
cancellable `SCAN` requests. It also estimates `LRANGE` replies before execution.

Full `redis-cli` option parity, cluster routing/discovery, replication/RDB
transfers, and estimates for commands other than `LRANGE` are not implemented.
See **Compatibility and limits** below for metadata and streaming limitations.

## Install and connect

Use Rust with edition 2024 support (the current toolchain is recommended):

```sh
cargo install --path . --locked
rmycli
rmycli -h localhost -p 6379 -n 2
rmycli -s /tmp/redis.sock
rmycli -u redis://localhost:6379/2
rmycli -u rediss://redis.example.com:6379/0
REDISCLI_AUTH=secret rmycli --user alice
```

`--tls` enables certificate-verified TLS for `-h`/`-p` connections. System trust
roots are used. A URL supplies connection settings; explicit `-n`, `--user`,
and `-a`/`REDISCLI_AUTH` override its database and credentials. `REDIS_URL` is
also supported. Use `rmycli --help` for all options (`-h` selects the host).

Commands can also be supplied directly or one per line on stdin:

```sh
rmycli PING
rmycli --raw GET greeting
printf 'MULTI\nSET greeting "hello world"\nEXEC\n' | rmycli
rmycli --yes LRANGE big-list 0 -1
```

Command-line arguments pass through as supplied by the operating system.
Interactive and stdin input support single/double quotes, backslash escapes,
and `\xHH` byte escapes. Suggestions round-trip through the same parser, so
spaces, quotes, backslashes, NULs, and non-UTF-8 key bytes remain intact.
Formatted output escapes terminal control sequences; `--raw` writes bulk
reply bytes verbatim in noninteractive mode. Redis errors exit noninteractive
mode with a nonzero status.

## Live help and editing

On connection, `rmycli` loads [COMMAND][command] and, when available,
[COMMAND DOCS][command-docs]. Remaining arguments appear in grey on the input
line, like `redis-cli`: typing `lrange` displays `lrange key start stop`, with
only `key start stop` supplied by the program. Typing the key changes the hint
to `start stop`. Hints never become part of the submitted command. They are
hidden while editing inside the line and clipped at the terminal's right edge.
F1 or `HELP command` shows full syntax, summary, version, group, and complexity.
Nested alternatives, optional
clauses, repeated arguments, subcommands, and module documentation come from
the server rather than a fixed set of handwritten command signatures.

The complete official Redis command-help catalog is bundled as a fallback.
Live documentation takes precedence, including for module commands. Fallback
help is labeled because it can differ from the connected server's version.
Command names are restricted to the live catalog when `COMMAND` succeeds.

| Key / command | Action |
| --- | --- |
| Tab / Shift-Tab | Start completion / cycle through popup candidates |
| Enter with popup open | Accept the candidate; does **not** execute |
| Enter without popup | Submit the command |
| Escape | Close the popup or cancel discovery/confirmation |
| Left / Right, Home / End, Ctrl-A / Ctrl-E | Move the cursor |
| Backspace / Delete, Ctrl-U / Ctrl-K | Delete characters or text before/after the cursor |
| Up / Down | Browse in-memory command history |
| F1 | Add the full current command help to the transcript |
| PageUp / PageDown | Scroll the transcript |
| Ctrl-L | Clear the transcript |
| Ctrl-C | Clear input; if executing/streaming, close that connection |
| Ctrl-D, `\quit`, `\q`, `QUIT`, `EXIT` | Exit |
| `HELP command`, `HELP @group` | Full command help or commands in a group |
| `HELP`, `\help` | Editor help |
| `\reconnect` | Open a new connection and refresh metadata |

Completion choices appear in a bordered floating window above the current
argument, overlaying the transcript without moving the input line. The selected
choice is highlighted; long menus scroll as you cycle through them. The window
stays within the terminal bounds. F1 and transcript scrolling make full help
accessible. The prompt is `rmycli>` for database zero and `rmycli[N]>` otherwise.
The shell uses an alternate terminal screen.
History is bounded to 500 commands and is not saved to disk. Pasting text never
submits a command. The transcript retains the latest 2,000 output lines.

## Key completion

For example:

```text
rmycli> LRANGE list:<Tab>
```

starts discovery on an independent connection with:

```text
SCAN 0 MATCH list:* COUNT 128 TYPE list
```

Redis key specifications determine argument positions: fixed indexes,
keyword-based starts, key counts, strides, and ranges. Argument trees help
identify key positions in unfinished commands, including trailing timeouts
and stream IDs that have not been typed yet. Legacy first/last/step metadata
is supported. Unknown or incomplete specifications can fall back to
`COMMAND GETKEYS` when enough of the command has been entered for Redis to
extract its keys; this introspection does not execute the command.

Separate type hints cover lists, hashes, sets, sorted sets, streams, strings,
bitmaps, and HyperLogLogs. Commands accepting multiple types, overwrite
operations, and unknown module types use unfiltered discovery. Existing-key
suggestions never prevent entering a new destination key. If the server
rejects `SCAN TYPE` syntax, discovery retries without that filter and labels
the candidates as unfiltered.

Completion escapes glob metacharacters in the typed prefix before adding
`*`. It preserves key case and bytes, deduplicates batches, and continues past
empty batches until cursor zero or a budget is reached. It never uses `KEYS`.

- Zero matches after a complete scan leaves the input unchanged.
- One match after a complete scan inserts that key.
- Multiple matches open a popup; Tab and Shift-Tab cycle, Enter accepts,
  and Escape closes it.
- Above the display threshold, confirmation is required before showing keys.
- Partial scans say **at least N matches (partial scan)**. Choose `y` to show
  discovered keys, `c` to continue discovery, or Enter/Escape/`n` to cancel.
  A partial scan with one candidate still requires explicit acceptance.

Default limits and their options:

| Limit | Default | Option |
| --- | --- | --- |
| Display confirmation threshold | 100 keys | `--completion-threshold` |
| Retained distinct keys | 10,000 | `--scan-max-keys` |
| Retained raw key bytes | 4 MiB | `--scan-max-bytes` |
| SCAN requests per discovery step | 32 | `--scan-max-batches` |
| Time per discovery step | 750 ms | `--scan-ms` |
| Connection / auxiliary socket timeout | 1,000 ms | `--timeout-ms` |

A continuation permits another threshold-sized page within the hard key/byte
limits. `COUNT` is only a Redis work hint. The client checks limits between
requests; an in-flight request can add up to its socket timeout, and a single
reply must be received before its retained-key budget can be checked. Editing
or pressing Escape cancels further requests; generation IDs discard stale
results. Network work runs outside the terminal input loop.

## Reply-size guidance and session state

Before ordinary `LRANGE` execution, an auxiliary `LLEN` request estimates the
number of elements in the inclusive range, including negative indexes and
bounds. Above `--reply-threshold` (default 10,000), interactive mode asks:

```text
Continue? [y/N] May return about 3000000 elements (not bytes; data can change).
```

Declining leaves the command unexecuted and available for editing. In
noninteractive mode, an oversized estimate causes an error before execution;
`--yes` explicitly bypasses the check. Unknown estimates, unsupported
commands, and failed probes remain unknown and permit manual execution.
These estimates are not memory limits or guarantees.

Discovery and estimation use separate connections with the active database
and credentials. They are paused during `MULTI`, `WATCH`, and streaming modes.
`SELECT` changes are tracked, including successful queued changes at `EXEC`;
`DISCARD` leaves the database unchanged. `AUTH`, `HELLO AUTH`, and `RESET`
update auxiliary connection settings. Probes never enter the user's transaction.
Commands queued inside transactions execute without reply-size confirmation.

Connection errors never trigger automatic command replay. Use `\reconnect`
explicitly; reconnecting starts a fresh session and does not restore transaction,
WATCH, subscription, or other connection-local state. Ctrl-C during execution
uses `CLIENT ID` and `CLIENT KILL ID` through the auxiliary connection to close
only this client's socket. If ACLs forbid cancellation, Ctrl-D exits the client.
Closing the socket cannot undo a command the server has already executed.

## Compatibility and limits

- Redis 7+ provides the richest metadata. Integration tests run against Redis
  8.0.2. Older metadata and ACL-restricted introspection use labeled bundled
  documentation and available legacy fields; they are tested with fixtures.
- A module must expose key specifications or argument documentation for
  completion of an unfinished command. `COMMAND GETKEYS` requires sufficient
  arguments and permission. Missing metadata never prevents manual submission.
- Some partial commands are intrinsically ambiguous: after `XREAD STREAMS k`,
  the next word could be another key or the first ID. Suggestions are advisory,
  not command validation. Unrecognized option ordering may reduce argument hints.
- A scan describes only the connected node and database. Redis Cluster routing,
  redirection following, and cluster-wide discovery are not implemented.
  Even a completed SCAN is not a snapshot of a changing keyspace.
- Pub/Sub and MONITOR replies stream to the transcript. In interactive streaming
  mode, Ctrl-C ends the stream; reconnect before submitting further commands.
  Replication protocols and an interactive unsubscribe workflow are not provided.
- History and the transcript are bounded, but command replies are decoded in
  memory before display. The `LRANGE` estimate is not a general reply-memory cap.
- No persistent history, SSH tunnels, custom TLS certificate flags, or full
  `redis-cli` option compatibility is claimed by this version.

## Development

The code separates byte parsing, command metadata and argument grammar, type
hints, scan discovery, estimation, session transport, workers, and the terminal.
Dependencies are `redis` for RESP and transport, `rustls` for the TLS crypto
provider, `crossterm` for terminal I/O, `clap` for options, `serde_json` for
metadata, and `unicode-width` for cursor/layout calculations.

Install `redis-server` for the isolated integration fixtures, which start their
own Unix-socket servers with persistence disabled. Tests never use an existing
Redis database. Before every commit, including documentation-only commits:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

To exercise the actual executable, terminal editing, completion acceptance,
cancellation, streaming, and noninteractive policy, also install `redis-cli`
and Python 3, then run:

```sh
cargo build --locked
python3 tests/terminal_smoke.py
```

Use the 80-column `rustfmt.toml` configuration and keep `Cargo.lock` tracked.
See [AGENTS.md](AGENTS.md), [CHANGELOG.md](CHANGELOG.md), and the bundled help's
[attribution and license](data/README.md).

[command]: https://redis.io/docs/latest/commands/command/
[command-docs]: https://redis.io/docs/latest/commands/command-docs/
