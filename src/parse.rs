//! Lenient C-style number parsing (`atoi`/`atof` semantics) for wire and console input.

fn trim_leading_space(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(s.len());
    &s[start..]
}

/// Parses a leading integer, ignoring trailing garbage; `0` when there is none.
pub fn atoi(s: &[u8]) -> i32 {
    let s = trim_leading_space(s);
    let (negative, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };

    let mut value: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10 + i64::from(b - b'0');
        if value > i64::from(i32::MAX) + 1 {
            break;
        }
    }
    let value = if negative { -value } else { value };
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Parses the longest leading floating point number, or `None` when the input
/// does not start with one.
pub fn float_prefix(s: &[u8]) -> Option<f64> {
    let s = trim_leading_space(s);
    let mut end = 0;
    if matches!(s.first(), Some(b'+' | b'-')) {
        end += 1;
    }

    let int_start = end;
    while end < s.len() && s[end].is_ascii_digit() {
        end += 1;
    }
    let mut has_digits = end > int_start;

    if s.get(end) == Some(&b'.') {
        let mut frac_end = end + 1;
        while frac_end < s.len() && s[frac_end].is_ascii_digit() {
            frac_end += 1;
        }
        if frac_end > end + 1 || has_digits {
            has_digits = true;
            end = frac_end;
        }
    }
    if !has_digits {
        return None;
    }

    if matches!(s.get(end), Some(b'e' | b'E')) {
        let mut exp_end = end + 1;
        if matches!(s.get(exp_end), Some(b'+' | b'-')) {
            exp_end += 1;
        }
        let exp_digits = exp_end;
        while exp_end < s.len() && s[exp_end].is_ascii_digit() {
            exp_end += 1;
        }
        if exp_end > exp_digits {
            end = exp_end;
        }
    }

    std::str::from_utf8(&s[..end]).ok()?.parse().ok()
}

/// Parses a leading floating point number, ignoring trailing garbage; `0.0` when there is none.
pub fn atof(s: &[u8]) -> f64 {
    float_prefix(s).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoi_matches_c_semantics() {
        assert_eq!(atoi(b"28"), 28);
        assert_eq!(atoi(b"  -12abc"), -12);
        assert_eq!(atoi(b"+7"), 7);
        assert_eq!(atoi(b"abc"), 0);
        assert_eq!(atoi(b""), 0);
        assert_eq!(atoi(b"99999999999"), i32::MAX);
        assert_eq!(atoi(b"-99999999999"), i32::MIN);
    }

    #[test]
    fn atof_matches_c_semantics() {
        assert_eq!(atof(b"1.5x"), 1.5);
        assert_eq!(atof(b".5"), 0.5);
        assert_eq!(atof(b"1."), 1.0);
        assert_eq!(atof(b"-2e3"), -2000.0);
        assert_eq!(atof(b"1e"), 1.0);
        assert_eq!(atof(b"."), 0.0);
        assert_eq!(atof(b"junk"), 0.0);
        assert_eq!(float_prefix(b"junk"), None);
        assert_eq!(float_prefix(b"10"), Some(10.0));
    }
}
