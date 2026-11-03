# redis-rs patch

`redis/` contains the published `redis` 0.32.7 crate, including its upstream
licenses. The only source change exposes `Connection::recv_message()`, calling
the existing `read(false)` implementation. Unlike `recv_response()`, an idle
timeout does not mark the next message for disposal. The existing parser keeps
partial frames across timeouts; TCP, Unix sockets, and TLS share this path.

This lets the interactive worker read unsolicited messages without sending
commands or changing transaction state. Remove this patch when upstream
provides an equivalent public API. Application tests cover idle timeouts,
fragmented frames, invalidations, and subsequent command replies.
