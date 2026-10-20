# Bundled Redis command documentation

`commands.json` is an unmodified snapshot of the Redis documentation project's
command catalog, downloaded on 2026-10-19 from:

https://github.com/redis/redis-doc/blob/master/commands.json

Copyright the Redis documentation contributors. The catalog is provided under
Creative Commons Attribution-ShareAlike 4.0 International; the upstream license
is reproduced in [REDIS-DOC-LICENSE](REDIS-DOC-LICENSE). These terms apply to the
bundled documentation, independently of the application's source code.

SHA-256 of this snapshot:

```text
ae409e47340fe9447325c57765991c605fd93ab7a3f7c7b69d7f0e3bc0ab4db7
```

The executable embeds this catalog for fallback help. Connected-server
`COMMAND` metadata controls available commands and key positions;
`COMMAND DOCS` replaces bundled documentation when available. To update,
replace the JSON and upstream license, update this attribution and checksum,
and run the repository's formatting, Clippy, and test checks. Do not describe
bundled documentation as authoritative for an older or newer server.
