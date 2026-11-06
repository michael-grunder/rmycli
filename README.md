# rmycli

A Rust Redis shell with live command help, metadata-driven argument completion,
and type-aware key completion.

## Status

The first interactive implementation is available. It connects to Redis,
executes arbitrary commands, discovers the server's command catalog, shows
syntax and argument help while editing, and completes keys using bounded,
cancellable `SCAN` requests. It also estimates `LRANGE` replies before execution.

Common `redis-cli` connection, scripting, and output options are supported,
including RESP3 and cluster redirects. Cluster-wide discovery, replication/RDB
transfers, and estimates for commands other than `LRANGE` are not implemented.
See **Compatibility and limits** below for metadata and streaming limitations.

## Install and connect

Use Rust 1.88 or newer (the current stable toolchain is recommended).
Building from source also requires a C compiler and `make` for the bundled
libsodium cryptography library.

Once version 0.1.0 is published to crates.io, install it with:

```sh
cargo install rmycli --locked
```

To install from a source checkout instead, run `cargo install --path . --locked`
in the project directory. Then connect to Redis:

```sh
rmycli
rmycli -3
rmycli -h localhost -p 6379 -n 2
rmycli -s /tmp/redis.sock
rmycli -u redis://localhost:6379/2
rmycli -u rediss://redis.example.com:6379/0
REDISCLI_AUTH=secret rmycli --user alice
```

`--tls` enables certificate-verified TLS for `-h`/`-p` connections or upgrades
an existing TCP URI without changing its host or port. System trust roots are
used unless `--cacert FILE` or `--cacertdir DIR` supplies PEM CA certificates.
Use `--cert FILE --key FILE` for a PEM client certificate chain and private key.
These flags also work with `rediss://` URLs and TLS instances. `--insecure`
explicitly disables certificate verification; verification is otherwise on.
A URL supplies connection settings; explicit `-n`, `--user`,
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
Formatted output double-quotes bulk strings, including values inside
collections, and escapes terminal control sequences. For example, `GET foo`
displays `"bar"`; status replies such as `OK` and `PONG` remain unquoted.
Redirected stdout uses raw output by default, like `redis-cli`; `--no-raw`
restores formatted output. `--raw` writes reply bytes verbatim in noninteractive
mode. Redis errors exit noninteractive mode with a nonzero status, with or
without `-e`.
Server errors show the server's message without the Redis library's preamble
(for example, `(error) syntax error` in the editor). Errors without a message
show their server error code. Client and connection errors retain their context.

## Shell completions

Dynamic completions cover command-line options, fixed values, file paths,
configured `--instance` names, and saved usernames for `--user` and
`--set-default-user`. Add the appropriate line to your shell's startup file
after installing `rmycli` on `PATH`:

```bash
# Bash: ~/.bashrc
source <(COMPLETE=bash rmycli)
```

```zsh
# Zsh: ~/.zshrc, after compinit (your shell framework may already run it)
autoload -Uz compinit; compinit
source <(COMPLETE=zsh rmycli)
```

```fish
# Fish: ~/.config/fish/completions/rmycli.fish
COMPLETE=fish rmycli | source
```

```powershell
# PowerShell: $PROFILE
$env:COMPLETE = 'powershell'
rmycli | Out-String | Invoke-Expression
Remove-Item Env:\COMPLETE
```

```elvish
# Elvish: your rc.elv startup file
eval (E:COMPLETE=elvish rmycli | slurp)
```

These use [clap's dynamic completion engine][clap-complete]. Keep the generation
command in your startup file so each new shell picks up changes after an
upgrade. Run the same command in an existing shell to refresh its completions.
The shell controls how Tab displays, cycles, or accepts matching candidates.

For example, if `alice` and `mike` are saved for `redis://127.0.0.1:7000`:

```text
rmycli -p 7000 --user <Tab>     # alice, mike
rmycli -p 7000 --user m<Tab>    # mike
```

Usernames come from the local plaintext credential store and the selected
instance's configured user; `--set-default-user` suggests only saved accounts.
Endpoint selection follows `-h`/`-p`, `-u` or
`REDIS_URL`, `-s`, `--tls`, and `--instance`, including explicit overrides.
Transport, host, and port must match the saved endpoint; `localhost` and
`127.0.0.1` remain distinct. `--config` selects the settings file as usual;
`--no-config` disables local username and instance suggestions.

Completion never connects to Redis, lists server ACLs, prompts for a password,
or writes settings. Encrypted credential stores are not unlocked for completion
and their usernames are not suggested, even if a legacy plaintext file remains.
Missing or invalid local files quietly omit their suggestions. Names containing
control characters are omitted because shells use those characters as completion
delimiters. Arbitrary usernames can still be entered manually. Redis command and
key completion remain features of the interactive editor.

[clap-complete]: https://docs.rs/clap_complete/latest/clap_complete/env/index.html

## Familiar redis-cli options

`rmycli --help` groups options by purpose and includes connection, scripting,
TLS, and configuration examples. `-h` selects the host; help is `--help`.
Options precede the command. Once a command starts, subsequent arguments pass
through unchanged, including `-1`, `--raw`, and other option-looking values.
Existing rmycli long names, including `--password`, remain supported.
The names and core semantics follow the [Redis CLI reference][redis-cli].

| Purpose | Supported redis-cli options |
| --- | --- |
| Connection | `-h`, `-p`, `-s`, `-u`, `-n`, `-t` |
| Authentication | `-a`, `--pass`, `--user`, `--askpass`, `--no-auth-warning` |
| Protocol and session | `-2`, `-3`, `--name`, `-c`, `--show-pushes yes/no` |
| TLS | `--tls`, `--cacert`, `--cacertdir`, `--cert`, `--key`, `--insecure` |
| Output | `--raw`, `--no-raw`, `--csv`, `--json`, `--quoted-json`, `-d`, `-D` |
| Scripting | `-x`, `-X`, `--quoted-input`, `-r`, `-i`, `--eval`, `-e` |
| Key listing | `--scan`, `--pattern`, `--quoted-pattern`, `--count` |
| Information | `--help`, `--version`, `-v`, `--verbose` |

```sh
rmycli -3 --name maintenance -h localhost -p 6379
rmycli --askpass --user alice
rmycli --tls --cacert ca.pem --cert client.pem --key client.key
rmycli --json HGETALL account:1
rmycli -r 5 -i 0.5 INCR counter
printf 'hello\n' | rmycli -x SET greeting
rmycli -D '' --raw DUMP key > key.dump
rmycli -X payload RESTORE key2 0 payload REPLACE < key.dump
rmycli --quoted-input GET '"key\x00with-nul"'
rmycli --eval script.lua key1 key2 , arg1 arg2
rmycli --scan --pattern 'user:*' --count 100
```

- RESP2 remains the default. `-3` negotiates RESP3 on connection; `--json`
  and `--quoted-json` also select RESP3 unless `-2` is given. `-2` and `-3`
  together are rejected. Auxiliary connections use RESP2 independently.
- `--pass`/`-a` takes precedence over `REDISCLI_AUTH`; `--askpass` overrides
  every password source. The prompt requires terminal stdin and disables echo;
  it cannot be combined with `-x`/`-X`. Use `REDISCLI_AUTH` with piped input.
  Explicit command-line credentials produce a warning unless suppressed.
- `-t` is a connection timeout in **seconds**, including fractions; `-t 0`
  uses the OS timeout. Without `-t`, the existing `--timeout-ms` default of
  1,000 ms applies. Auxiliary connections stay bounded by `--timeout-ms` even
  with `-t 0`. Commands that intentionally block still have no read timeout.
- `-x` appends all stdin bytes, including trailing newlines, as one argument.
  `-X TAG` replaces the first argument matching TAG and rejects a missing tag
  or replacement of the command name. `--quoted-input` decodes each supplied
  argument separately, including `\xHH` escapes.
- `-r N` repeats a command in noninteractive mode, with `-i` seconds between
  executions. `-r 0` executes nothing; `-r -1` repeats until Ctrl-C.
  `--eval FILE` sends `EVAL`, using the standalone comma to separate keys
  from arguments; without a comma all trailing arguments are keys.
- `-d` separates raw array elements and `-D` separates raw replies. Empty
  delimiters are allowed. JSON preserves valid UTF-8; invalid UTF-8 falls back
  to ASCII byte escapes. `--quoted-json` always represents byte escapes as
  literal JSON strings. Nonfinite doubles become strings and nested errors
  become objects so output remains valid JSON. CSV uses redis-cli's escaped
  strings, rather than RFC 4180 quoting. The live editor always escapes
  terminal control bytes, even with `--raw`; JSON and CSV work in the editor.
- `--show-pushes` controls unsolicited RESP3 messages (default: on for terminal
  stdout). The async session reader receives messages while idle and wakes the
  worker when a message or submitted command arrives. Invalidations appear
  without another command, in bold, while preserving the input and cursor. The
  default interactive format is `-> invalidate: 'key'`, with one line per key, or
  `-> invalidate: (nil)` for a cache-wide invalidation. Keys are escaped safely;
  binary keys use double quotes and byte escapes. JSON, CSV, and noninteractive
  output retain their existing formats. Reading sends no Redis commands and
  also runs during WATCH and MULTI. Hidden pushes are
  still drained; Pub/Sub messages remain visible. Push styling is currently
  fixed. Noninteractive push display remains command driven.
- `--scan` streams one SCAN batch at a time until cursor zero without retaining
  the whole keyspace. Ctrl-C interrupts it; `-i` throttles requests. `--count`
  is a Redis work hint, not a result limit. Like redis-cli, this explicit listing
  may contain duplicates and is not a snapshot. It does not use the bounded
  completion budgets and makes no unique-key-count claim.
- Cluster mode is detected automatically using `INFO SERVER` after connecting.
  Set top-level `auto-cluster = false` in `rmycli.toml`, or run
  `rmycli config set auto-cluster false`, to disable automatic cluster mode.
  `-c` explicitly enables MOVED/ASK following even when detection is disabled
  or denied by ACLs. ASK retries send ASKING first.
- Valkey connections automatically advertise `CLIENT CAPA REDIRECT` when
  supported, enabling standalone replicas to redirect commands to their primary
  during failover. Valkey uses `REDIRECT host:port`, without a cluster slot.
  This works independently of `auto-cluster` and `-c`, including with `--scan`.
  Use `READONLY` to deliberately read from a Valkey replica. See
  [CLIENT CAPA](https://valkey.io/commands/client-capa/).
- Detection and capability negotiation run after authentication and on each
  redirected connection. Unsupported or ACL-denied probes leave manual commands
  available; detection is retried after successful AUTH/HELLO AUTH or RESET,
  once MULTI/WATCH ends. Automatic redirects require TCP or TLS; Unix sockets
  keep direct-connection behavior, and `-c` cannot be combined with `-s`.
- Redirect following stops after five redirects. It preserves TLS,
  authentication, database, protocol, and client name, but does not restore other
  connection-local state. Network failures are never retried, and commands are
  never redirected while MULTI/WATCH or streaming is active. This supports
  server-directed failover, not guaranteed recovery from a lost connection.
  Discovery and estimates use separate direct connections to the current node;
  they do not negotiate redirects or discover the whole cluster.

### Deliberately omitted and currently unsupported options

Use `redis-cli` for its separate administration and diagnostic tools:
`--cluster` and its sub-options; `--replica`/`--slave`, `--rdb`,
`--functions-rdb`; `--pipe`/`--pipe-timeout`; `--stat`, `--latency*`,
`--intrinsic-latency`, `--lru-test`, `--vset-recall*`; `--bigkeys`, `--memkeys*`,
`--keystats*`, `--hotkeys` and their `--cursor`/`--top` modifiers; and
`--ldb`/`--ldb-sync-mode`. These are distinct workflows beyond the interactive
shell and ordinary command execution.

Address-family selection (`-4`/`-6`), a separate `--sni`, `--tls-ciphers`, and
`--tls-ciphersuites` are also currently unsupported: rmycli does not expose
those transport controls. TLS uses the connection hostname for SNI and rustls
cipher defaults. Unsupported switches before the command fail with a usage
error; they are never accepted as no-ops. Full option parity is
not claimed.

The session uses asynchronous TCP, Unix sockets, or TLS, with the published
Redis 1.8 parser and command encoder; no vendored dependency is required.
A dedicated reader keeps partial frames across waits and feeds a bounded
64-message channel. User commands remain serialized on the same connection,
including protocol changes, transactions, RESP2/RESP3 Pub/Sub, and `MONITOR`.
This raw stream is needed because redis-rs's async multiplexer does not expose
arbitrary streaming replies on an existing connection. Dropping a session
aborts its reader; Ctrl-C still kills the exact server client to cancel a
blocking command. Completion, estimation, and metadata use separate
synchronous connections with timeouts on their worker thread.

## Configuration and saved instances

Settings are read once at startup from the first available file, in this order:

1. `$XDG_CONFIG_HOME/rmycli.toml`, or `~/.config/rmycli.toml` when
   `XDG_CONFIG_HOME` is unset, empty, or relative.
2. `rmycli.toml` in each directory in `$XDG_CONFIG_DIRS`, in order (default:
   `/etc/xdg`). Relative directory entries are ignored.

Use `--config /path/to/settings.toml` to select a file explicitly, or
`--no-config` to use built-in settings and ignore saved authentication. Files are
not merged. Missing discovered
files are skipped; a missing explicit file, unreadable file, malformed TOML,
unknown setting, or invalid value causes an error before connecting. No file is
created during normal startup. `--help` and `--version` work without loading
settings.

See [rmycli.dist.toml](rmycli.dist.toml) for a commented reference covering every
supported setting, defaults, color formats, and authentication examples. Copy
it to `~/.config/rmycli.toml` (or `$XDG_CONFIG_HOME/rmycli.toml`); the distribution
file is not loaded automatically.

For a shorter example, create `~/.config/rmycli.toml`:

```toml
[theme]
inline-hint = "dark_grey"

[theme.commands]
GET = "cyan"
LRANGE = "#89b4fa"
XGROUP = "green"
"XGROUP CREATE" = "yellow"

[instances.qa]
host = "redis.qa.example.com"
port = 6379
db = 0
user = "alice"
password-env = "QA_REDIS_PASSWORD"
tls = false
```

Select an instance with `rmycli --instance qa` (also works with a command, such
as `rmycli --instance qa PING`). Authentication happens automatically on connect,
including background completion and reply-estimation connections. Instances
are selected explicitly, not inferred from host/port. Instance names are case
sensitive. An unknown instance is an error.

All instance fields are optional: host and port default to `127.0.0.1:6379`,
database to zero, and TLS to off. Omit `user` for Redis's default user or
password-only authentication. Set `tls = true` for certificate-verified TLS.
Use either `password = "..."` or `password-env = "ENVIRONMENT_VARIABLE"`;
the latter reads the password from that variable when the instance is selected.
A missing password variable is an error unless a higher-priority password is
provided. Protect a file containing passwords with owner-only permissions.

Explicit `-h`, `-p`, `-n`, `--user`, and `-a`/`--pass` override the selected instance's
values. `REDISCLI_AUTH` overrides its password too; `-a` takes precedence over
that environment variable. `--tls` can enable TLS on an instance. `--instance`
cannot be combined with `-u`/`REDIS_URL` or `-s`; unset `REDIS_URL` before
selecting an instance. Without `--instance`, connection behavior is unchanged.
Connection timeouts and discovery/reply limits remain command-line options.

### Prompt customization

The default prompt uses cyan connection details and a green Unicode `❯`, such
as `alice@localhost:6379[2] ❯ `. Set the top-level `prompt` string **before any
TOML table headings**, or use the configuration commands:

```sh
rmycli config set prompt '{color:cyan}{address}{reset} db={db} {ctrl}❯ '
rmycli config get prompt
rmycli config unset prompt
```

Restart an open shell to load changes. Named placeholders are case sensitive:

| Placeholder | Value |
| --- | --- |
| `{user}` | Session username, or `default` |
| `{address}` | Connection host:port or Unix socket path |
| `{host}`, `{port}` | Separate host and port; Unix uses the path and an empty port |
| `{db}` | Current database number, including zero |
| `{database}` | `[N]` for a nonzero database, otherwise empty |
| `{transport}` | `redis`, `rediss`, or `unix` |
| `{ctrl}` | `[Ctrl]` while held on terminals supporting modifier events |
| `{color:cyan}` | Set foreground color for following prompt text |
| `{reset}` | Restore the terminal's default foreground color |

Colors accept the same names, `#RRGGBB`, `rgb_(r,g,b)`, and `ansi_(n)` formats
as theme settings. `NO_COLOR` suppresses colors when nonempty. Styles end before
command input; command echoes retain the expanded prompt text without colors.
Connection fields update with successful session changes and redirects, and
never include passwords. Ctrl-R retains its dedicated history prompt.

Use `{{` and `}}` for literal braces. Unknown placeholders, invalid colors,
unmatched braces, and control characters (including newlines and ANSI escapes)
are errors before connection. Templates are single-line; they do not execute
shell commands or expand environment variables. An empty template hides the
prompt. A plain ASCII alternative matching the older prompt is:

```toml
prompt = "{user}@{address}{database}{ctrl}> "
```

See the extensive **Interactive prompt** comments in
[rmycli.dist.toml](rmycli.dist.toml) for copyable recipes, every placeholder,
color formats, escaping rules, and limitations.

### Saved server authentication

When a server requires authentication, the interactive shell says so and uses
bundled metadata until you authenticate with `AUTH password`,
`AUTH username password`, or `HELLO 3 AUTH username password`. Successful
authentication reloads `COMMAND` and `COMMAND DOCS` for completions and help,
and supplies credentials to background connections. Failed authentication does
not refresh metadata or save credentials. Refresh waits until MULTI/WATCH ends;
queued authentication is applied only when EXEC reports it succeeded. Servers
that deny introspection through ACLs still use bundled metadata.

Credentials can be kept separately in `$XDG_CONFIG_HOME/rmycli-auth.toml`
(normally `~/.config/rmycli-auth.toml`). This user-only lookup also applies when
`--config` selects a different main settings file. Add server stanzas manually:

```toml
[servers."redis://127.0.0.1:7000"]
# Omit user for password-only authentication.
user = "default"
password = "replace-me"

[servers."rediss://redis.example.com:6379"]
# Optional when several accounts are saved; must name a saved account.
default-user = "alice"

[servers."rediss://redis.example.com:6379".users]
alice = "alice-password"
bob = "bob-password"

# Other endpoint forms:
# [servers."redis://[::1]:6379"]
# password = "ipv6-password"
# [servers."unix:/absolute/path/to/redis.sock"]
# password = "socket-password"
```

Entries match the transport, hostname, and port (or socket path) exactly, with
no wildcard or DNS alias matching. Use lowercase hostnames without a trailing
dot and include the port; databases share credentials. `localhost` and
`127.0.0.1` are separate entries, as are TLS and plain TCP. Socket paths match
as supplied. Each endpoint can store multiple accounts, with one password per
username. Existing `user`/`password` entries remain supported. A password-only
entry represents Redis's `default` user; it can coexist with other users in the
`users` table. Do not repeat the same account in both forms.

A sole saved password or username/password pair is selected automatically.
With multiple accounts and no requested user or saved default, the first
account in alphabetical username order is selected automatically. Use `--user`
to choose another account or set a saved default:

```sh
rmycli -h redis.example.com --tls --user bob
rmycli -h redis.example.com --tls --set-default-user alice
rmycli -h redis.example.com --tls
```

`--set-default-user USER` updates the saved choice and exits without connecting.
It requires an already saved account, works even when automatic saving is off,
and accepts the usual endpoint selectors (`-h`/`-p`, `-u`, `-s`, `--instance`,
and `--tls`). Use `--set-default-user default` for a saved password-only account
or an ACL account named `default`. You can also edit `default-user` in the auth
file; remove it to return to automatic first-account selection. Saving or
updating other accounts does not change the selected default.

Existing URL, instance, CLI, environment, and `--askpass` passwords take
precedence over saved credentials.
An explicit username (including one from a URL or instance) overrides the saved
default and uses a saved password only if that account matches. An unknown
username does not fall back to another account's password; you can still
authenticate manually.
`--no-config` disables both reading and writing this file.

Automatic saving is **disabled by default**. Set the top-level `save-auth` in
the main `rmycli.toml`, before any table headings:

```toml
save-auth = false     # Default: never save automatically.
# save-auth = "local"  # Save only for localhost, loopback IPs, or Unix sockets.
# save-auth = "always" # Save for every server.
```

Alternatively, run `rmycli config set save-auth local`. Use `false` or `always`
to select the other policies, or `rmycli config unset save-auth` to restore the
default. TOML requires quotes around `"local"` and `"always"`.

Saving applies after successful connection-time authentication and successful
AUTH/HELLO AUTH commands, including stdin commands. It updates that account's
password while retaining other accounts, defaults, and server entries. `local`
recognizes `localhost`, IPv4/IPv6 loopback addresses, and Unix sockets; it does
not resolve arbitrary hostnames or include private network addresses. Changing the policy
to `false` stops future automatic saves but still reads existing entries. Delete
an account to forget it (and remove `default-user` if it names that account);
RESET does not delete saved credentials.

MOVED, ASK, and Valkey REDIRECT connections reuse the session's credentials.
After a successful redirected connection, those credentials are also saved for
the destination endpoint if `save-auth` permits it. Later direct connections to
that node can then use its saved credentials. Failed authentication never saves
credentials for the destination.

By default, saved passwords are stored as **plaintext**. Protect manually created auth files with
`chmod 600 ~/.config/rmycli-auth.toml`. Automatic writes replace files atomically
with owner-only permissions on Unix and follow existing symlinks. Invalid files
are reported without echoing their contents and are never overwritten. A save
failure leaves the authenticated connection usable and prints a warning.
Concurrent saves use a short-lived `.toml.lock` file; a conflicting save warns
instead of overwriting another writer. If a process crashes during a save,
remove its stale lock after ensuring no other process is saving. Binary,
non-UTF-8 credentials execute unchanged but cannot be saved or reused by
background connections or reconnects.

### Encrypted saved authentication

To encrypt credentials from the first save, put this before any table headings
in `rmycli.toml` (or use `rmycli config set encrypt-auth true`):

```toml
encrypt-auth = true
save-auth = "local" # Or "always"; encryption does not enable automatic saving.
```

The encrypted store is `$XDG_CONFIG_HOME/rmycli/auth.enc`, normally
`~/.config/rmycli/auth.enc`. The first successful authentication that needs to
save credentials asks for a new master passphrase twice, with terminal echo
disabled. Later sessions prompt when saved credentials are first needed.
An existing encrypted file takes precedence even if `encrypt-auth` is false;
its magic, version, and algorithms are validated before unlocking. A malformed
file or failed unlock never falls back to plaintext. `--no-config` disables
both stores. Explicit connection passwords bypass saved-password lookup.

To migrate all accounts and saved default-user choices from the existing
plaintext auth file, run:

```sh
rmycli auth encrypt
```

This creates a new encrypted file and refuses to overwrite an existing one.
It asks for and confirms a master passphrase, then prints a warning naming the
plaintext file that remains. Verify a new connection works, then delete that
plaintext file yourself. Existing shells should be restarted after migration.
Setting `encrypt-auth = true` alone starts a separate store; it does not import
or delete plaintext credentials. Migration does not move passwords stored in
`[instances]` in `rmycli.toml`; use environment variables for those passwords.

Change the master passphrase without connecting to Redis:

```sh
rmycli auth passwd
```

In the interactive shell, use `\auth lock`, `\auth unlock`, or `\auth passwd`.
Locking discards the shared store's key and cached credentials. The next store
access requires unlocking again. It does **not** close Redis connections or
clear credentials already copied into connection settings for reconnects and
background assistance. Exit the CLI to discard that session as well.
`--set-default-user` works with either storage backend.

Master passphrases are accepted only through hidden terminal prompts, never
command-line options or environment variables. Escape or Ctrl-C cancels a
prompt. Passphrases may contain up to 1,024 UTF-8 bytes. A mismatch or empty
new passphrase is rejected without writing. Piped stdin cannot unlock or create
a store: use explicit connection credentials and disable automatic saving for
unattended commands. If saving fails after
Redis authentication succeeds, the connection stays usable and reports the
failure; no plaintext fallback is written.

The backend uses libsodium through [Alkali](https://docs.rs/alkali/0.3.0/).
Argon2id 1.3 derives a 256-bit key with a random 16-byte salt, initially using
64 MiB and two passes (one lane). XChaCha20-Poly1305-IETF encrypts the complete
JSON credential payload with a fresh random 24-byte nonce on every write.
The exact header is authenticated as associated data; the 16-byte tag follows
the ciphertext. Authentication failures report `wrong passphrase or damaged
file`. Passphrase changes generate a new salt, key, and nonce.

Version 1 has a fixed 60-byte header; integers are unsigned, big-endian:

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 8 | Magic: `RMYAUTH` followed by a NUL byte |
| 8 | 2 | Format version: 1 |
| 10 | 1 | KDF: 1 = libsodium Argon2id 1.3, one lane |
| 11 | 1 | Encryption: 1 = XChaCha20-Poly1305-IETF |
| 12 | 4 | KDF memory limit in bytes |
| 16 | 4 | KDF operation limit |
| 20 | 16 | Random salt |
| 36 | 24 | Random nonce |
| 60 | variable | Encrypted JSON payload followed by authentication tag |

Readers reject files larger than 8 MiB, unsupported formats, and KDF costs
outside 64–256 MiB or 2–6 passes before deriving a key. Writes use a private
0700 application directory and exclusive 0600 temporary files containing only
encrypted data. File operations reject symlinks, hard links, wrong ownership,
and unsafe permissions. Writes fsync the temporary file, rename it, and fsync
the directory. A stable `auth.lock` file serializes writers; do not delete it.
A session whose snapshot changed refuses to overwrite another session's work;
lock and unlock again to reload, then retry the update.

Passphrase and plaintext buffers and cached passwords are zeroed on drop;
keys use libsodium's protected allocation with best-effort memory locking.
Redis and serialization libraries can hold additional copies; complete removal
of secrets from process memory or crash dumps is not guaranteed. Credential
commands and credential-bearing Redis URLs are excluded from saved history,
recalled history, and submitted-command transcripts. Text typed into the
ordinary Redis command editor remains visible while editing.

The encrypted file is portable between supported Unix machines: copy it into
a directory owned by the destination user with mode 0700, and keep the file
mode 0600. Losing the master passphrase loses access to its credentials.
Encryption protects stored data, not an unlocked process on a compromised
account or machine. Retained plaintext files, backups, and configuration
passwords remain plaintext. An OS keychain backend is not implemented.

The theme controls the foreground color of inline argument hints. Command
names are case insensitive, with whitespace normalized; a subcommand's color
overrides its parent's color, which overrides `inline-hint`. Matching uses the
resolved metadata command, not key or argument text. Unknown command names
are allowed so the same file can cover module commands on different servers.
Duplicate names after normalization are errors. Popup colors are unchanged.

Colors accept `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
`white`, `grey`, `dark_grey`, and the `dark_` variants of red, green, yellow,
blue, magenta, and cyan. Also supported: `reset` (terminal default),
`"#RRGGBB"`, `"rgb_(R,G,B)"`, and `"ansi_(N)"` (components/index 0–255).
For example, `inline-hint = "#ffffff"` selects white; hex digits accept either
case. Keep the quotes because an unquoted `#` starts a TOML comment.
Named colors follow the terminal palette; RGB rendering depends on terminal
support. The built-in hint color remains `dark_grey`.

### Read and edit settings from the command line

Local configuration commands run without connecting to Redis:

```sh
rmycli config get theme.inline-hint
rmycli config set theme.inline-hint cyan
rmycli config set theme.commands.LRANGE '#89b4fa'
rmycli config set 'theme.commands."XGROUP CREATE"' yellow
rmycli config unset theme.commands.LRANGE
rmycli config set instances.qa.host redis.qa.example.com
rmycli config set instances.qa.port 6380
rmycli config set instances.qa.password-env QA_REDIS_PASSWORD
rmycli config get instances.qa.host
```

`get` prints the effective value, including built-in defaults and parent-command
color inheritance. Missing optional settings and unknown instances return an
error. It reads the file's settings, without applying connection flags or
resolving password environment variables. `set` prints the saved value;
password updates print `[redacted]`. An explicit `get` of a stored password
prints it.

For color settings, both `get` and `set` show the value followed by `sample text`
rendered in that color when stdout is a terminal. Redirected output is just the
value and a newline. A nonempty `NO_COLOR` disables automatic previews.
Use `--color always` to force a preview (including when `NO_COLOR` is set), or
`--color never` to suppress it:

```sh
rmycli config get theme.inline-hint --color always
rmycli config get theme.inline-hint --color never
```

Keys use TOML dotted-key syntax; quote a component containing spaces or dots,
as in `'theme.commands."XGROUP CREATE"'` or `'instances."qa.eu".host'`.
String values are literal arguments, requiring only shell quoting. Ports and
databases must be integers; TLS takes `true` or `false`. The complete updated
file must pass validation before it is saved. `unset` removes an explicit value,
restoring its default/inheritance, and fails if that value was not set.

Edits preserve comments and unrelated settings and replace the file atomically.
By default they write `$XDG_CONFIG_HOME/rmycli.toml` (normally
`~/.config/rmycli.toml`), creating directories as needed. If only a system file
exists, its contents are copied into the new user file before editing, so the
other effective settings are retained. System files are not edited implicitly.
`--config PATH`, before or after `config`, selects a specific file; `set` can
create it. Existing symlinks are followed, existing file permissions are retained,
and new files have owner-only permissions on Unix. Changes take effect in new
shell sessions.

Lowercase `rmycli config` is reserved for these local operations. Use uppercase
`rmycli CONFIG GET ...` or `rmycli CONFIG SET ...` for the Redis command.
Interactive Redis commands are unaffected. `rmycli config --help` lists the
local commands; `rmycli --no-config config get ...` reads built-in defaults.

## Live help and editing

On connection, `rmycli` loads [COMMAND][command] and, when available,
[COMMAND DOCS][command-docs]. Remaining arguments appear in grey by default on
the input line, like `redis-cli`: typing `lrange` displays `lrange key start stop`, with
only `key start stop` supplied by the program. Typing the key changes the hint
to `start stop`. Hints never become part of the submitted command. They are
hidden while editing inside the line and clipped at the terminal's right edge.
Redraws are buffered and use synchronized terminal updates where supported,
so clearing hints, printing output, and drawing the next prompt appear together.
F1 opens a dismissible help panel with current command documentation, editor
shortcuts, and connection status. `HELP command` prints full syntax, summary,
version, group, and complexity to the terminal.
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
| Ctrl+Enter on `KEYS <pattern>` | Stream matching keys with `SCAN` |
| Hold Ctrl (with modifier-event support) | Show `[Ctrl]` in the prompt |
| Escape | Close the popup or cancel discovery/confirmation |
| Left / Right, Home / End, Ctrl-A / Ctrl-E | Move the cursor |
| Backspace / Delete, Ctrl-U / Ctrl-K | Delete characters or text before/after the cursor |
| Up / Down | Browse command history (per-server by default) |
| Ctrl-R | Search command history with fuzzy matching |
| F1 | Open or close command help, editor shortcuts, and connection details |
| PageUp / PageDown, Up / Down with help open | Scroll the help panel |
| Enter with help open | Close help without submitting input |
| `CLEAR`, Ctrl-L | Clear the screen locally without sending a Redis command |
| Ctrl-C | Stop alternate `KEYS` scanning; send `UNSUBSCRIBE` while subscribed; otherwise clear input or close an executing/streaming connection |
| Ctrl-D, `\quit`, `\q`, `QUIT`, `EXIT` | Exit |
| `HELP command`, `HELP @group` | Full command help or commands in a group |
| `HELP`, `\help` | Open editor help and connection details |
| `\reconnect` | Open a new connection and refresh metadata |

Ctrl-R opens a history popup, using the current input as the initial query.
Type to filter, use Ctrl-R / Down / Tab for the next match and Up / Shift-Tab
for the previous match. Enter recalls the selection for editing; press Enter
again to execute it. Esc or Ctrl-C closes search and restores the original
input and cursor. An empty query shows all retained commands; no matches leaves
the search open. Duplicate commands appear once, newest first.

Interactive history loads from `$XDG_CONFIG_HOME/rmycli_history`, normally
`~/.config/rmycli_history`, and saves on exit. One JSON-lines file retains up to
500 entries **total across all servers**, dropping the oldest saved entries.
By default, only the initial endpoint's entries appear in Up/Down and Ctrl-R;
databases and accounts on that endpoint share history. Endpoint matching uses
the same transport, hostname, port, and socket-path rules as saved authentication.
Redirects keep the initial endpoint's history. New commands from overlapping
sessions merge with the latest file on exit, ordered by save time. Commands
from another session become visible after restarting.

Set top-level `history-per-server = false` in `rmycli.toml` to show all servers'
entries together in Up/Down and Ctrl-R, in saved order. The default is `true`.
You can also run `rmycli config set history-per-server false`, then restart the
shell. Entries still store their server tags, so switching back restores
per-server filtering without changing the history file format.

The history file location is independent of settings discovery, including
`--no-config` and `--config`. `--no-config` uses the default per-server filtering.
Noninteractive commands are not recorded. Writes are atomic and
owner-only on Unix; existing symlinks are followed. Invalid files are left
untouched, and load/save failures warn without preventing shell use. A
simultaneous save warns instead of overwriting another writer; after a crash,
remove a stale `rmycli_history.lock` only when no process is saving. Commands
from a killed or crashed session may be lost because saving happens on exit.

History contains plaintext command arguments. `AUTH`, `HELLO`, `ACL SETUSER`,
`CONFIG SET`, `MIGRATE` with authentication, and credential-bearing Redis URLs
are excluded from disk history, in-memory recall, and submitted-command
transcripts. Existing sensitive history entries are skipped on load and omitted
on the next history save. Other commands can still contain sensitive keys or
values. These exclusions do not change command execution.

Search supports fuzzy terms and the leading-apostrophe convention from
[fzf](https://junegunn.github.io/fzf/search-syntax/): `gusr` matches
`GET user:42`, while `'user` requires the contiguous substring `user`.
Space-separated terms must all match, in any order; `get 'user` combines both
styles. Escape a space with a backslash to keep it inside a term:
`'GET\ user` searches for the exact substring `GET user`. Lowercase terms
ignore case; a term containing uppercase letters is case sensitive. The
apostrophe is a prefix, not a pair of shell quotes. Other fzf operators and
fzf's relevance scoring are not implemented; results stay newest first.

Ctrl-R uses the ordinary terminal control character, including inside tmux;
it does not require extended keys or the Kitty keyboard protocol. No tmux
configuration changes are needed with its default bindings. A custom tmux
binding or terminal shortcut that intercepts Ctrl-R can still prevent delivery.

Experiment with `KEYS *` or `KEYS foo:*` followed by **Ctrl+Enter** to stream
keys using `SCAN` until cursor zero. The pattern is passed through unchanged:
`KEYS foo:*` uses `SCAN ... MATCH foo:* COUNT 128`. Empty batches do not stop
iteration. Output is streamed with bounded buffering; duplicates are possible
and the listing is not a snapshot. It covers the current database on the
connected node, without completion budgets or a unique-key-count claim.

**Ctrl-C stops further iterations without exiting or disconnecting.** Any
in-flight request finishes first; buffered keys are discarded, and the output
is labeled partial. You can then run another command on the same connection.
Alternate `KEYS` is unavailable during MULTI, WATCH, or streaming sessions.
Normal Enter still sends `KEYS` directly; there is no default-remapping setting
yet. With a completion popup open, either Enter only accepts the candidate.

Ctrl+Enter requires a terminal that distinguishes it from Enter. The editor
requests the Kitty keyboard protocol, including modifier presses and releases.
On supporting terminals, holding either Ctrl key changes
`default@127.0.0.1:6379 ❯` to `default@127.0.0.1:6379 [Ctrl]❯`; releasing it restores
the prompt. Modifier events leave the input, completion popup, and confirmation
unchanged. Shift remains available
for typing case-sensitive keys. Ctrl+Enter with a popup open only accepts the
candidate; release events never submit commands.

Terminals or multiplexers without this support may send plain Enter for both,
which executes ordinary `KEYS`. They may also omit standalone Ctrl presses and
releases, so the held-key indicator cannot work there. An ordinary Ctrl chord
alone does not turn on the indicator, since its release may never be reported.

If modified Enter acts like Enter, check the whole terminal path, including
tmux or screen. For example, tmux with `extended-keys off` cannot forward the
distinction. Changing the application shortcut alone cannot fix that. Try a
terminal supporting the Kitty keyboard protocol directly, outside tmux, to
test the held-key indicator. See the [keyboard protocol][keyboard-protocol]
and [tmux modifier-key troubleshooting][tmux-modifiers]. The application does
not modify terminal or tmux configuration.

[keyboard-protocol]: https://sw.kovidgoyal.net/kitty/keyboard-protocol/
[tmux-modifiers]: https://github.com/tmux/tmux/wiki/Modifier-Keys

The prompt starts at the terminal's current cursor row and follows replies,
like a conventional command-line shell. If existing output ends mid-line,
the prompt starts on the next line. There is no startup banner or persistent
connection footer. Startup shows a metadata-loading status only after 200 ms
or when you type, avoiding a flash on fast connections.
The shell uses the normal terminal buffer, so earlier shell output and command
replies remain in your terminal's scrollback, including
after exit. Use your terminal's scrollback controls to review output.

Completion choices appear in a bordered window below the current argument.
The selected choice is highlighted; long menus scroll as you cycle through
them. Short status messages share the input line when it is empty. Help,
completion, and longer messages reserve space below the prompt only when
visible, scrolling as needed at the bottom of the terminal.
Closing a panel removes it and preserves the input. Errors, discovery progress,
and confirmations still appear when needed. The prompt shows the session user,
host, and port, such as `alice@127.0.0.1:6379 ❯`. The username defaults to `default`
when none is supplied and updates after successful AUTH or HELLO AUTH.
Nonzero databases add a suffix, such as `alice@127.0.0.1:6379[2] ❯`; database zero
has no suffix. Unix connections show the socket path instead of host and port.

In-memory history is bounded to 500 commands. Pasting text never
submits a command. Output retention is controlled by your terminal's scrollback
settings; the application does not keep a separate transcript.

## Key completion

For example:

```text
default@127.0.0.1:6379 ❯ LRANGE list:<Tab>
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

Interactive sessions reconnect quietly after connection loss or Ctrl-C during
execution. If reconnection fails, the error is shown; submitting a new command
or using `\reconnect` tries again. Interrupted commands are never replayed.
Reconnection reuses the current endpoint, database, credentials, protocol, and
client name, but starts a fresh session: transaction, WATCH, subscription, and
other connection-local state are not restored. Ctrl-C closes the active socket
locally, without requiring `CLIENT ID` or `CLIENT KILL` permissions. Closing the
socket cannot undo a command the server has already executed.

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
- A scan describes only the connected node and database. `-c` follows command
  redirects; slot-map routing and cluster-wide discovery are not implemented.
  Even a completed SCAN is not a snapshot of a changing keyspace.
- Pub/Sub replies stream while the interactive prompt accepts further commands,
  including additional subscriptions. Redis enforces the allowed commands in
  RESP2; RESP3 permits ordinary commands while subscribed. Ctrl-C at the subscribed
  prompt sends `UNSUBSCRIBE` without arguments, removing all ordinary channel
  subscriptions and keeping the connection and session state. Pattern and shard
  subscriptions remain until `PUNSUBSCRIBE` and `SUNSUBSCRIBE`, respectively.
  Background assistance resumes once all subscriptions end, unless MULTI or
  WATCH is still active. Ctrl-C during an executing command still closes the
  connection to cancel it.
- MONITOR displays server-formatted lines without extra quoting or an input
  prompt. Ctrl-C stops the stream and quietly reconnects before restoring the
  prompt. Terminal control characters remain escaped. Replication protocols
  are not provided.
- History is bounded and displayed output is released by the application, but
  command replies are decoded in memory before display. The `LRANGE` estimate
  is not a general reply-memory cap.
- SSH tunnels and full `redis-cli` option compatibility are not implemented.
  See the option compatibility section above.

## Development

The code separates byte parsing, command metadata and argument grammar, type
hints, scan discovery, estimation, session transport, workers, and the terminal.
Dependencies are `redis` for RESP and auxiliary connections, `tokio` for async
session I/O, and `combine` for the decoder state required by Redis's public
async parser. `tokio-rustls`, `rustls`, and `rustls-native-certs` provide TLS,
its crypto provider, and system trust roots. `crossterm` handles terminal I/O,
`clap` handles options, `serde_json` handles metadata, and `unicode-width`
handles cursor/layout calculations. `toml` and `serde` parse typed configuration
and reject unknown fields and invalid values;
`xdg` resolves configuration directories. Crossterm's Serde support validates
color values. `toml_edit` preserves comments during CLI edits, and `tempfile`
provides temporary files for atomic replacement.

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

When `tmux` is installed, the smoke suite also checks Ctrl-R search in a
private tmux server with no user configuration. Linux CI installs tmux for this
check. The test does not connect to or modify existing tmux sessions.

Use the 80-column `rustfmt.toml` configuration and keep `Cargo.lock` tracked.
See [AGENTS.md](AGENTS.md), [CHANGELOG.md](CHANGELOG.md), and the bundled help's
[attribution and license](data/README.md).

### GitHub Actions and distribution binaries

[CI](.github/workflows/ci.yml) runs formatting, Clippy, and the existing unit,
CLI, TLS, and isolated Redis integration tests on Linux and macOS for pushes
and pull requests. Linux also runs the Python terminal smoke suite. Redis is
installed by the workflows; each test starts its own disposable server.

[Distribution binaries](.github/workflows/dist.yml) runs when a `v*` tag is
pushed, or manually from the Actions tab. It produces these downloadable
workflow artifacts:

| Archive | Contents |
| --- | --- |
| `rmycli-x86_64-unknown-linux-musl.tar.gz` | Static Linux x86-64 binary |
| `rmycli-aarch64-unknown-linux-musl.tar.gz` | Static Linux ARM64 binary |
| `rmycli-universal-apple-darwin.tar.gz` | macOS 11+ Intel and Apple Silicon binary |

Linux builds use musl on native runners for each architecture. The workflow
runs the Rust and terminal suites against the release builds and rejects ELF
binaries with a dynamic interpreter or shared-library dependencies. TLS still
needs system CA certificates or an explicit `--cacert` file at runtime.

The macOS build combines both architectures with `lipo`, verifies both slices,
and applies an ad-hoc signature. Rust tests run on Apple Silicon; the Intel
slice is compiled but not executed. The result is not Developer ID signed or
notarized.

Each artifact contains a tar archive and its SHA-256 checksum. The archive
includes `rmycli`, the README, licenses, and the sample configuration. Extract
it and put `rmycli` on your PATH. These workflows upload Actions artifacts;
they do not publish GitHub Releases or crates.io packages.

To run terminal tests against a specific binary locally:

```sh
RMYCLI_TEST_BINARY=target/x86_64-unknown-linux-musl/release/rmycli \
  python3 tests/terminal_smoke.py
```

### Publishing

For your first release, sign in to [crates.io](https://crates.io), verify your
email address, create an API token with permission to publish, and run
`cargo login`. Paste the token at Cargo's prompt.

Before publishing, move the release's changelog entries from `Unreleased` into
a version heading dated `YYYY-MM-DD`, retaining an `Unreleased` section. Run the
formatting, Clippy, and test checks above and commit the release changes. Then:

```sh
cargo package --list
cargo publish --dry-run --locked
cargo publish --locked
```

Review the package file list before uploading. The manifest explicitly includes
source, bundled help and its license, tests, and project documentation; local
recordings are excluded. The dry run builds the packaged source without
uploading it. After a successful publish, tag the release commit (for example,
`git tag v0.1.0`). A published version cannot be overwritten; corrections need
a new version. See the [Cargo publishing guide][publishing] for details.

## License

The application source code is licensed under the [MIT License](LICENSE).
The bundled Redis command documentation in `data/commands.json` is separately
licensed under CC BY-SA 4.0; see its [attribution and license](data/README.md).

[publishing]: https://doc.rust-lang.org/cargo/reference/publishing.html

[command]: https://redis.io/docs/latest/commands/command/
[command-docs]: https://redis.io/docs/latest/commands/command-docs/

[redis-cli]: https://redis.io/docs/latest/develop/tools/cli/
