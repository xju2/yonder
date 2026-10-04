//! The host aliases in `~/.ssh/config`, for the connect screen to offer.

use std::path::{Path, PathBuf};

/// The `Host` aliases in `config` and the files it `Include`s, in order and
/// without repeats. Patterns (`*`, `?`, `!`) are left out: they are not
/// destinations. A missing or unreadable file gives no hosts.
pub fn hosts(config: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let ssh_dir = config.parent().unwrap_or(Path::new("."));
    read(config, ssh_dir, &mut out, 0);
    out
}

fn read(file: &Path, ssh_dir: &Path, out: &mut Vec<String>, depth: u8) {
    // ssh itself stops at 16 levels; a loop of Includes should not hang us.
    if depth > 16 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        // `Keyword value` or `Keyword=value`, keywords in any case.
        let (key, rest) = line
            .split_once(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or((line, ""));
        let values = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        if key.eq_ignore_ascii_case("host") {
            for name in values.split_whitespace() {
                if !name.contains(['*', '?', '!']) && !out.iter().any(|h| h == name) {
                    out.push(name.to_string());
                }
            }
        } else if key.eq_ignore_ascii_case("include") {
            for pattern in values.split_whitespace() {
                for f in expand(pattern, ssh_dir) {
                    read(&f, ssh_dir, out, depth + 1);
                }
            }
        }
    }
}

/// An `Include` argument as files: `~` is home, relative paths are in
/// `~/.ssh`, and `*` or `?` may appear in the file name (not in directories).
fn expand(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Vec::new(),
        },
        None => ssh_dir.join(pattern),
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| matches(name.as_bytes(), e.file_name().as_encoded_bytes()))
        .map(|e| e.path())
        .collect();
    // ssh reads matches in sorted order.
    files.sort();
    files
}

fn matches(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            matches(&pattern[1..], name) || (!name.is_empty() && matches(pattern, &name[1..]))
        }
        (Some(b'?'), Some(_)) => matches(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => matches(&pattern[1..], &name[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_from_config_and_includes() {
        let d = std::env::temp_dir().join(format!("yonder-sshcfg-{}", std::process::id()));
        std::fs::create_dir_all(d.join("config.d")).unwrap();
        std::fs::write(
            d.join("config"),
            "# comment\nInclude config.d/*.conf\nHost pl perlmutter\n  HostName x\n\
             Host *.nersc.gov !bad\nhost=dtn\nHOST pl\nMatch host foo\n",
        )
        .unwrap();
        std::fs::write(d.join("config.d/b.conf"), "Host bee\n").unwrap();
        std::fs::write(d.join("config.d/a.conf"), "Host ay\n").unwrap();
        std::fs::write(d.join("config.d/skip.txt"), "Host no\n").unwrap();
        assert_eq!(
            hosts(&d.join("config")),
            ["ay", "bee", "pl", "perlmutter", "dtn"]
        );
        assert!(hosts(&d.join("missing")).is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }
}
