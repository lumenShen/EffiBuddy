//! Tests for the clipboard file-list parsing that lives in GPUI.
//!
//! `vendor/gpui` is a patched registry dependency, not a workspace member, so
//! its unit tests cannot run under `cargo test`. The parser is exported
//! (`gpui::parse_uri_list_paths`) precisely so its behaviour stays covered
//! from here — pasting a file from a GTK/Qt file manager depends on it.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use gpui::parse_uri_list_paths;

    /// Scratch directory; the ui crate has no `tempfile` dependency and does
    /// not need one for three tests.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "effibuddy-uri-list-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A copy in a file manager puts one percent-encoded URI per line on the
    /// clipboard, with `\r\n` endings and optional comments.
    #[test]
    fn parses_percent_encoded_file_uris() {
        let dir = ScratchDir::new("plain");
        let plain = dir.path().join("notes.txt");
        let spaced = dir.path().join("screen shot.png");
        std::fs::write(&plain, b"hi").unwrap();
        std::fs::write(&spaced, b"png").unwrap();

        let payload = format!(
            "# copied from nautilus\r\nfile://{}\r\nfile://{}\r\n",
            plain.display(),
            spaced.display().to_string().replace(' ', "%20")
        );
        assert_eq!(
            parse_uri_list_paths(payload.as_bytes()),
            vec![plain, spaced]
        );
    }

    /// A dragged link shares `text/uri-list` but is not a file, so the composer
    /// must fall back to text instead of staging a bogus attachment.
    #[test]
    fn rejects_non_file_uris_and_missing_paths() {
        assert!(parse_uri_list_paths(b"https://example.com/a.png\r\n").is_empty());
        assert!(parse_uri_list_paths(b"file:///definitely/not/here.txt\r\n").is_empty());
        assert!(parse_uri_list_paths(b"").is_empty());
        assert!(parse_uri_list_paths(b"# only a comment\r\n").is_empty());
        assert!(parse_uri_list_paths(b"file://remote-host/share/a.png\r\n").is_empty());
    }

    /// Paths with CJK names are percent-encoded byte by byte; decoding must
    /// round-trip them.
    #[test]
    fn decodes_non_ascii_paths() {
        let dir = ScratchDir::new("cjk");
        let cjk = dir.path().join("截图 2026.png");
        std::fs::write(&cjk, b"png").unwrap();
        let encoded: String = cjk
            .to_string_lossy()
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect();
        let payload = format!("file://{encoded}\r\n");
        assert_eq!(parse_uri_list_paths(payload.as_bytes()), vec![cjk]);
    }
}
