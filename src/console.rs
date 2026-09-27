//! Console output and the `developer` verbosity level.

use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};

static DEVELOPER: AtomicI32 = AtomicI32::new(0);

pub fn developer() -> i32 {
    DEVELOPER.load(Ordering::Relaxed)
}

pub fn set_developer(level: i32) {
    DEVELOPER.store(level, Ordering::Relaxed);
}

pub fn print(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

#[macro_export]
macro_rules! cprint {
    ($($arg:tt)*) => {
        $crate::console::print(&format!($($arg)*))
    };
}

/// Prints only when the `developer` cvar is non-zero.
#[macro_export]
macro_rules! dprint {
    ($($arg:tt)*) => {
        if $crate::console::developer() != 0 {
            $crate::console::print(&format!($($arg)*));
        }
    };
}

/// Renders Quake's extended character set (colored digits, brackets, line
/// drawing glyphs, high-bit "bronze" text) as plain ASCII.
pub fn qstr(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            let c = match b {
                146..=155 => b - 146 + b'0',
                143 => b'.',
                157..=159 => b'-',
                128.. => b - 128,
                _ => b,
            };
            match c {
                16 => '[',
                17 => ']',
                29..=31 => '-',
                7 => ' ',
                c => c as char,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::qstr;

    #[test]
    fn maps_quake_glyphs_to_ascii() {
        assert_eq!(qstr(b"plain"), "plain");
        assert_eq!(qstr(&[146, 147, 155]), "019");
        assert_eq!(qstr(&[b'a' | 128, 16, 17, 29, 7]), "a[]- ");
        assert_eq!(qstr(&[157, 158, 159, 143]), "---.");
    }
}
