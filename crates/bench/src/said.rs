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
    while value >= 1000.0 && unit + 1 < UNITS.len() {
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
    let s = d.as_secs_f64();
    if s >= 1.0 {
        format!("{s:.2} s")
    } else if s >= 0.001 {
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
        assert_eq!(bytes(31_200_000), "31.2 MB");
        assert_eq!(time(Duration::from_millis(1234)), "1.23 s");
        assert_eq!(time(Duration::from_micros(45_100)), "45.1 ms");
        assert_eq!(time(Duration::from_micros(812)), "812 µs");
    }
}
