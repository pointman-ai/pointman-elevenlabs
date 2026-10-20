//! How numbers, sizes, times and names read in what the extension says.

use std::time::Duration;

/// 12345 as "12,345".
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// A file's size: "812 KB", "24.3 MB".
pub fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1_000_000 => format!("{} KB", b.div_ceil(1000)),
        b if b < 1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b => format!("{:.2} GB", b as f64 / 1e9),
    }
}

/// Time so far: "40 s", "3 min 05 s", "1 h 02 min".
pub fn took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {:02} s", s / 60, s % 60),
        s => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

/// A day from Unix seconds, as "2026-11-03" (UTC).
pub fn day(unix: i64) -> String {
    // Howard Hinnant's days-to-civil
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A name for a file: lowercase letters, digits and hyphens, at most `max` characters.
pub fn slug(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= max {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "audio".into()
    } else {
        out
    }
}

/// A file name's stem: "talk" for "talk.v2.wav" is "talk.v2".
pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn they_read_well() {
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-1000), "-1,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(size(24_300_000), "24.3 MB");
        assert_eq!(took(Duration::from_secs(185)), "3 min 05 s");
        assert_eq!(day(1_760_000_000), "2025-10-09");
        assert_eq!(slug("Rachel — calm, 2", 40), "rachel-calm-2");
        assert_eq!(slug("!!", 40), "audio");
        assert_eq!(stem("talk.v2.wav"), "talk.v2");
    }
}
