//! Command-specific hints supplement server argument metadata.
//! Unknown types and commands accepting more than one type stay unfiltered.
use crate::input::Context;

pub fn config_parameter_pattern(ctx: &Context) -> Option<Vec<u8>> {
    let [command, subcommand, ..] = &ctx.args[..ctx.index] else {
        return None;
    };
    if !command.eq_ignore_ascii_case(b"CONFIG")
        || !(subcommand.eq_ignore_ascii_case(b"GET")
            || (subcommand.eq_ignore_ascii_case(b"SET")
                && ctx.index.is_multiple_of(2)))
    {
        return None;
    }
    let mut pattern = ctx.prefix.clone();
    if !pattern
        .iter()
        .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
    {
        pattern.push(b'*');
    }
    Some(pattern)
}

pub fn key_type(command: &str) -> Option<&'static str> {
    let root = command.split(' ').next().unwrap_or_default();
    match root {
        "BLMOVE" | "BLMPOP" | "BLPOP" | "BRPOP" | "BRPOPLPUSH" | "LINDEX"
        | "LINSERT" | "LLEN" | "LMOVE" | "LMPOP" | "LPOP" | "LPOS"
        | "LPUSH" | "LPUSHX" | "LRANGE" | "LREM" | "LSET" | "LTRIM"
        | "RPOP" | "RPOPLPUSH" | "RPUSH" | "RPUSHX" => Some("list"),
        "HDEL" | "HEXISTS" | "HGET" | "HGETALL" | "HINCRBY"
        | "HINCRBYFLOAT" | "HKEYS" | "HLEN" | "HMGET" | "HMSET"
        | "HRANDFIELD" | "HSCAN" | "HSET" | "HSETNX" | "HSTRLEN" | "HVALS"
        | "HEXPIRE" | "HPEXPIRE" | "HEXPIREAT" | "HPEXPIREAT" | "HTTL"
        | "HPTTL" | "HEXPIRETIME" | "HPEXPIRETIME" | "HPERSIST" | "HGETDEL"
        | "HGETEX" | "HSETEX" => Some("hash"),
        "SADD" | "SCARD" | "SDIFF" | "SINTER" | "SINTERCARD" | "SISMEMBER"
        | "SMEMBERS" | "SMISMEMBER" | "SMOVE" | "SPOP" | "SRANDMEMBER"
        | "SREM" | "SSCAN" | "SUNION" => Some("set"),
        "BZPOPMAX" | "BZPOPMIN" | "BZMPOP" | "ZADD" | "ZCARD" | "ZCOUNT"
        | "ZINCRBY" | "ZLEXCOUNT" | "ZMPOP" | "ZMSCORE" | "ZPOPMAX"
        | "ZPOPMIN" | "ZRANDMEMBER" | "ZRANGE" | "ZRANGEBYLEX"
        | "ZRANGEBYSCORE" | "ZRANK" | "ZREM" | "ZREMRANGEBYLEX"
        | "ZREMRANGEBYRANK" | "ZREMRANGEBYSCORE" | "ZREVRANGE"
        | "ZREVRANGEBYLEX" | "ZREVRANGEBYSCORE" | "ZREVRANK" | "ZSCAN"
        | "ZSCORE" | "GEOADD" | "GEODIST" | "GEOHASH" | "GEOPOS"
        | "GEOSEARCH" => Some("zset"),
        "XACK" | "XADD" | "XAUTOCLAIM" | "XCLAIM" | "XDEL" | "XGROUP"
        | "XINFO" | "XLEN" | "XPENDING" | "XRANGE" | "XREAD" | "XREADGROUP"
        | "XREVRANGE" | "XSETID" | "XTRIM" | "XACKDEL" | "XDELEX" => {
            Some("stream")
        }
        "APPEND" | "DECR" | "DECRBY" | "GET" | "GETDEL" | "GETEX"
        | "GETRANGE" | "INCR" | "INCRBY" | "INCRBYFLOAT" | "STRLEN"
        | "SUBSTR" | "SETRANGE" | "GETBIT" | "SETBIT" | "BITCOUNT"
        | "BITFIELD" | "BITFIELD_RO" | "BITPOS" | "PFADD" | "PFCOUNT" => {
            Some("string")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::context;

    #[test]
    fn config_completion_targets_names_and_preserves_explicit_globs() {
        for (line, pattern) in [
            ("config get ", b"*".as_slice()),
            ("CoNfIg sEt slow", b"slow*"),
            ("CONFIG SET save '60 1' slow", b"slow*"),
            ("CONFIG GET save 'slowlog-*'", b"slowlog-*"),
            ("CONFIG GET *max-len", b"*max-len"),
            ("CONFIG GET sl?w", b"sl?w"),
            ("CONFIG GET [sm]*", b"[sm]*"),
            (r"CONFIG GET 'a\*'", b"a\\*"),
        ] {
            assert_eq!(
                config_parameter_pattern(&context(line, line.len())).as_deref(),
                Some(pattern),
                "{line}"
            );
        }
        for line in [
            "CONFIG",
            "CONFIG G",
            "CONFIG HELP ",
            "CONFIG SET save ",
            "CONFIG SET save '' maxmemory 1",
            "GET slowlog-",
        ] {
            assert!(
                config_parameter_pattern(&context(line, line.len())).is_none(),
                "{line}"
            );
        }
    }

    #[test]
    fn types_and_mixed_type_commands() {
        assert_eq!(key_type("LRANGE"), Some("list"));
        assert_eq!(key_type("XGROUP CREATE"), Some("stream"));
        assert_eq!(key_type("ZUNION"), None);
        assert_eq!(key_type("SET"), None);
        assert_eq!(key_type("MODULE.CUSTOM"), None);
    }
}
