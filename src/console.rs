//! Console output and the `developer` verbosity level.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

use rustyline::ExternalPrinter;

static DEVELOPER: AtomicI32 = AtomicI32::new(0);

/// Output goes through the interactive line editor when one is running, so
/// log lines do not garble the command being typed.
struct Interactive {
    printer: Box<dyn ExternalPrinter + Send>,
}

static INTERACTIVE: Mutex<Option<Interactive>> = Mutex::new(None);

pub fn developer() -> i32 {
    DEVELOPER.load(Ordering::Relaxed)
}

pub fn set_developer(level: i32) {
    DEVELOPER.store(level, Ordering::Relaxed);
}

/// Routes subsequent output through an interactive line editor.
pub fn set_printer(printer: Box<dyn ExternalPrinter + Send>) {
    if let Ok(mut guard) = INTERACTIVE.lock() {
        *guard = Some(Interactive { printer });
    }
}

pub fn print(text: &str) {
    if let Ok(mut guard) = INTERACTIVE.lock()
        && let Some(interactive) = guard.as_mut()
    {
        if interactive.printer.print(text.to_string()).is_ok() {
            return;
        }
        // The editor is gone; fall back to plain stdout from now on.
        *guard = None;
    }

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
