//! Numbers as the output says them: counts with separators, sizes in
//! decimal units, times in the unit that keeps three figures.

use std::time::Duration;

/// `2104331` as "2,104,331".
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `31_200_000` as "31.2 MB".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    // A unit up where the figure, rounded to the tenth said, reaches 1000:
    // 999,950 B is "1.0 MB", never "1000.0 KB".
    while unit + 1 < UNITS.len()
        && (if unit == 0 {
            value
        } else {
            (value * 10.0).round() / 10.0
        }) >= 1000.0
    {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A time as "1.23 s", "45.1 ms" or "812 µs".
pub fn time(d: Duration) -> String {
    // The unit chosen by the figure as it is rounded: 999.96 ms is "1.00 s",
    // 999.6 µs is "1.0 ms".
    let s = d.as_secs_f64();
    if s >= 1.0 || (s * 1e3 * 10.0).round() >= 10_000.0 {
        format!("{s:.2} s")
    } else if s >= 0.001 || (s * 1e6).round() >= 1000.0 {
        format!("{:.1} ms", s * 1e3)
    } else {
        format!("{:.0} µs", s * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_as_the_output_says_them() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(2_104_331), "2,104,331");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(999_950), "1.0 MB");
        assert_eq!(bytes(999_940), "999.9 KB");
        assert_eq!(time(Duration::from_micros(999_960)), "1.00 s");
        assert_eq!(time(Duration::from_nanos(999_600)), "1.0 ms");
        assert_eq!(bytes(31_200_000), "31.2 MB");
        assert_eq!(time(Duration::from_millis(1234)), "1.23 s");
        assert_eq!(time(Duration::from_micros(45_100)), "45.1 ms");
        assert_eq!(time(Duration::from_micros(812)), "812 µs");
    }
}
