//! Value-type hints are deliberately separate from server key-position metadata.
//! Unknown types and commands accepting more than one type stay unfiltered.
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
    #[test]
    fn types_and_mixed_type_commands() {
        assert_eq!(key_type("LRANGE"), Some("list"));
        assert_eq!(key_type("XGROUP CREATE"), Some("stream"));
        assert_eq!(key_type("ZUNION"), None);
        assert_eq!(key_type("SET"), None);
        assert_eq!(key_type("MODULE.CUSTOM"), None);
    }
}
