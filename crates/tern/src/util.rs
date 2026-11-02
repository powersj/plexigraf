//! Small helpers shared by config, encodings, and the runtime.

use std::time::Duration;

/// Match `*` (any run of characters) and `?` (one character).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

pub fn is_glob(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// Parse a Go-style duration such as `10s`, `1m30s`, or `250ms`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let mut total = 0f64;
    let mut rest = s;
    if rest.is_empty() {
        return Err("empty duration".into());
    }
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| format!("duration {s:?} is missing a unit (for example \"10s\")"))?;
        let (num, after) = rest.split_at(num_len);
        let unit_len = after
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(after.len());
        let (unit, next) = after.split_at(unit_len);
        let n: f64 = num.parse().map_err(|_| format!("invalid duration {s:?}"))?;
        let scale = match unit {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(format!("invalid unit {unit:?} in duration {s:?}")),
        };
        total += n * scale;
        rest = next;
    }
    Ok(Duration::from_secs_f64(total))
}

/// A little randomness for jitter, without pulling in an RNG crate.
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("web*", "web1"));
        assert!(glob_match("web*", "web"));
        assert!(glob_match("*_total", "requests_total"));
        assert!(glob_match("w?b", "web"));
        assert!(!glob_match("web*", "api"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("a*c", "abd"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5400));
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10x").is_err());
    }
}
