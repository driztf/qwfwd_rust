//! Quake info strings: `\key\value\key\value...` byte strings.

use crate::console::qstr;
use crate::cprint;

pub const MAX_INFO_STRING: usize = 1024;
pub const MAX_INFO_KEY: usize = 64;

/// Rejects info strings with empty keys or values.
pub fn validate(userinfo: &[u8]) -> bool {
    let mut i = 0;
    while i < userinfo.len() {
        if userinfo[i] == b'\\' {
            i += 1;
        }
        match userinfo.get(i) {
            Some(b'\\') => return false,
            Some(_) => i += 1,
            None => break,
        }
        while i < userinfo.len() && userinfo[i] != b'\\' {
            i += 1;
        }
    }
    true
}

pub fn value_for_key<'a>(s: &'a [u8], key: &[u8]) -> &'a [u8] {
    if s.first() != Some(&b'\\') || key.is_empty() {
        return &[];
    }
    let mut parts = s[1..].split(|&b| b == b'\\');
    while let Some(k) = parts.next() {
        let Some(v) = parts.next() else { break };
        if k == key {
            return v;
        }
    }
    &[]
}

/// Byte range of the `\key\value` pair for `key`, including its leading backslash.
fn pair_span(s: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    loop {
        let start = i;
        if s.get(i) == Some(&b'\\') {
            i += 1;
        }
        let key_start = i;
        while i < s.len() && s[i] != b'\\' {
            i += 1;
        }
        if i >= s.len() {
            return None;
        }
        let k = &s[key_start..i];
        i += 1;
        while i < s.len() && s[i] != b'\\' {
            i += 1;
        }
        if k == key {
            return Some((start, i));
        }
        if i >= s.len() {
            return None;
        }
    }
}

pub fn remove_key(s: &mut Vec<u8>, key: &[u8]) {
    if key.contains(&b'\\') {
        return;
    }
    if let Some((start, end)) = pair_span(s, key) {
        s.drain(start..end);
    }
}

/// Sets `key` to `value`, refusing changes that would not fit in `maxsize`.
/// Star keys are allowed; `check_key_len` enforces the usual 64 byte limit.
pub fn set_value_for_star_key(
    s: &mut Vec<u8>,
    key: &[u8],
    value: &[u8],
    maxsize: usize,
    check_key_len: bool,
) {
    if key.contains(&b'\\')
        || value.contains(&b'\\')
        || key.contains(&b'"')
        || value.contains(&b'"')
    {
        return;
    }
    if check_key_len && (key.len() >= MAX_INFO_KEY || value.len() >= MAX_INFO_KEY) {
        return;
    }

    let existing = value_for_key(s, key);
    if !existing.is_empty()
        && value.len() as isize - existing.len() as isize + s.len() as isize + 1 > maxsize as isize
    {
        return;
    }

    remove_key(s, key);
    if value.is_empty() {
        return;
    }

    let mut pair = Vec::with_capacity(key.len() + value.len() + 2);
    pair.push(b'\\');
    pair.extend_from_slice(key);
    pair.push(b'\\');
    pair.extend_from_slice(value);
    if pair.len() + s.len() + 1 > maxsize {
        return;
    }
    s.extend(pair.into_iter().filter(|&c| c > 13));
}

pub fn set_value_for_key(
    s: &mut Vec<u8>,
    key: &[u8],
    value: &[u8],
    maxsize: usize,
    check_key_len: bool,
) {
    if key.first() == Some(&b'*') {
        cprint!("Can't set * keys\n");
        return;
    }
    set_value_for_star_key(s, key, value, maxsize, check_key_len);
}

pub fn print(s: &[u8]) {
    let mut rest = s.strip_prefix(b"\\").unwrap_or(s);
    while !rest.is_empty() {
        let key_end = rest.iter().position(|&b| b == b'\\').unwrap_or(rest.len());
        cprint!("{:<20} ", qstr(&rest[..key_end]));
        if key_end == rest.len() {
            cprint!("MISSING VALUE\n");
            return;
        }
        rest = &rest[key_end + 1..];
        let value_end = rest.iter().position(|&b| b == b'\\').unwrap_or(rest.len());
        cprint!("{}\n", qstr(&rest[..value_end]));
        rest = &rest[(value_end + 1).min(rest.len())..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_pairs() {
        assert!(validate(b"\\name\\foo\\prx\\host"));
        assert!(validate(b""));
        assert!(!validate(b"\\name\\\\prx\\host"));
        assert!(!validate(b"\\\\foo"));
    }

    #[test]
    fn looks_up_values() {
        let s = b"\\name\\foo\\prx\\host:27500\\*ver\\1";
        assert_eq!(value_for_key(s, b"name"), b"foo");
        assert_eq!(value_for_key(s, b"prx"), b"host:27500");
        assert_eq!(value_for_key(s, b"*ver"), b"1");
        assert_eq!(value_for_key(s, b"missing"), b"");
        assert_eq!(value_for_key(b"name\\foo", b"name"), b"");
        assert_eq!(value_for_key(b"\\name", b"name"), b"");
    }

    #[test]
    fn removes_keys() {
        let mut s = b"\\name\\foo\\prx\\host\\team\\red".to_vec();
        remove_key(&mut s, b"prx");
        assert_eq!(s, b"\\name\\foo\\team\\red");
        remove_key(&mut s, b"name");
        assert_eq!(s, b"\\team\\red");
        remove_key(&mut s, b"team");
        assert_eq!(s, b"");
    }

    #[test]
    fn sets_and_replaces_values() {
        let mut s = b"\\name\\foo".to_vec();
        set_value_for_key(&mut s, b"prx", b"host", MAX_INFO_STRING, true);
        assert_eq!(s, b"\\name\\foo\\prx\\host");
        set_value_for_key(&mut s, b"name", b"bar", MAX_INFO_STRING, true);
        assert_eq!(s, b"\\prx\\host\\name\\bar");
        set_value_for_key(&mut s, b"name", b"", MAX_INFO_STRING, true);
        assert_eq!(s, b"\\prx\\host");
        set_value_for_star_key(&mut s, b"*qwfwd", b"1.40", MAX_INFO_STRING, true);
        assert_eq!(s, b"\\prx\\host\\*qwfwd\\1.40");
    }

    #[test]
    fn refuses_oversized_and_malformed_values() {
        let mut s = b"\\a\\b".to_vec();
        set_value_for_key(&mut s, b"x", b"y", 5, true);
        assert_eq!(s, b"\\a\\b");
        set_value_for_key(&mut s, b"q\"", b"y", MAX_INFO_STRING, true);
        assert_eq!(s, b"\\a\\b");
        set_value_for_key(&mut s, b"k", &[b'v', 1, b'w'], MAX_INFO_STRING, true);
        assert_eq!(s, b"\\a\\b\\k\\vw");
    }
}
