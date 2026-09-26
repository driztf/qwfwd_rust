//! Config file lookup restricted to the working directory.

/// Absolute paths, drive letters and parent directory references are rejected.
pub fn safe_path(path: &str) -> bool {
    !(path.starts_with(['\\', '/']) || path.contains("..") || path.as_bytes().get(1) == Some(&b':'))
}

/// File extension including the dot, or empty when there is none.
pub fn file_extension(path: &str) -> &str {
    path.rfind('.').map_or("", |i| &path[i..])
}

/// Reads a config file from `qwfwd/`, falling back to the working directory.
pub fn read_config(name: &str) -> Option<Vec<u8>> {
    ["qwfwd", ""].iter().find_map(|dir| {
        let path = if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        };
        if !safe_path(&path) {
            return None;
        }
        std::fs::read(path).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_paths() {
        assert!(safe_path("qwfwd.cfg"));
        assert!(safe_path("configs/qwfwd.cfg"));
        assert!(!safe_path("/etc/passwd"));
        assert!(!safe_path("\\windows"));
        assert!(!safe_path("../secret.cfg"));
        assert!(!safe_path("c:stuff.cfg"));
    }

    #[test]
    fn extracts_extension() {
        assert_eq!(file_extension("qwfwd.cfg"), ".cfg");
        assert_eq!(file_extension("a.b.CFG"), ".CFG");
        assert_eq!(file_extension("noext"), "");
    }
}
