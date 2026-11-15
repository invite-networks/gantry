//! Parsers for Compose's duration and byte size strings.

use std::time::Duration;

/// Parses Compose durations: `10s`, `1m30s`, `1h`, `500ms`, `2.5s`, `90us`.
/// A bare number is seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(secs) = s.parse::<f64>() {
        return secs_to_duration(secs, s);
    }
    let mut total = 0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| format!("invalid duration {s:?}: missing unit"))?;
        if num_end == 0 {
            return Err(format!("invalid duration {s:?}"));
        }
        let value: f64 = rest[..num_end]
            .parse()
            .map_err(|_| format!("invalid duration {s:?}"))?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_end] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            u => return Err(format!("invalid duration {s:?}: unknown unit {u:?}")),
        };
        total += value * factor;
        rest = &rest[unit_end..];
    }
    secs_to_duration(total, s)
}

fn secs_to_duration(secs: f64, original: &str) -> Result<Duration, String> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(format!("invalid duration {original:?}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

/// Parses byte sizes: `512m`, `1g`, `1024k`, `100b`, `2gb`, or plain bytes.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let num_end = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let value: f64 = lower[..num_end]
        .parse()
        .map_err(|_| format!("invalid size {s:?}"))?;
    let factor: u64 = match lower[num_end..].trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        u => return Err(format!("invalid size {s:?}: unknown unit {u:?}")),
    };
    let bytes = value * factor as f64;
    if !bytes.is_finite() || bytes < 0.0 {
        return Err(format!("invalid size {s:?}"));
    }
    Ok(bytes as u64)
}

/// Formats a duration the way docker and podman CLIs accept (`1500ms`).
pub fn cli_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2.5s").unwrap(), Duration::from_millis(2500));
        assert_eq!(parse_duration("15").unwrap(), Duration::from_secs(15));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("s").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_bytes("512m").unwrap(), 512 << 20);
        assert_eq!(parse_bytes("1g").unwrap(), 1 << 30);
        assert_eq!(parse_bytes("1GB").unwrap(), 1 << 30);
        assert_eq!(parse_bytes("100").unwrap(), 100);
        assert!(parse_bytes("12q").is_err());
    }

    #[test]
    fn cli_format() {
        assert_eq!(cli_duration(Duration::from_secs(30)), "30s");
        assert_eq!(cli_duration(Duration::from_millis(1500)), "1500ms");
    }
}
