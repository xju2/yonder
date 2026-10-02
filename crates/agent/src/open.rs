//! `yonder FILE...`: open files in the app from one of its terminals.
//!
//! The agent links itself as `yonder` on the terminal's `PATH` (see
//! `pty::command_dir`). Run under that name, it prints an escape sequence per
//! file that the app's terminal catches; nothing goes over a socket.

use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

pub fn run(args: &[String]) -> i32 {
    if std::env::var_os("YONDER_TERMINAL").is_none() {
        eprintln!("yonder: run this in a Yonder terminal");
        return 1;
    }
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: yonder FILE...\nOpens files in the Yonder window this terminal belongs to.");
        return 2;
    }
    let tmux = std::env::var_os("TMUX").is_some();
    let mut out = io::stdout().lock();
    let mut code = 0;
    for arg in args {
        match sequence(arg, tmux) {
            Ok(seq) => {
                let _ = out.write_all(seq.as_bytes());
            }
            Err(e) => {
                eprintln!("yonder: {arg}: {e}");
                code = 1;
            }
        }
    }
    let _ = out.flush();
    code
}

fn sequence(arg: &str, tmux: bool) -> Result<String, String> {
    let path = normalize(&std::path::absolute(arg).map_err(|e| e.to_string())?);
    let meta = path.metadata().map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("is a folder; open files only".into());
    }
    let path = path.to_str().ok_or("not a UTF-8 path")?;
    if path.chars().any(char::is_control) {
        return Err("contains control characters".into());
    }
    Ok(osc(path, tmux))
}

/// `ESC ] 7777 ; open ; PATH BEL`. Inside tmux it is wrapped for passthrough,
/// which tmux 3.3+ drops unless `allow-passthrough` is on.
fn osc(path: &str, tmux: bool) -> String {
    let seq = format!("\x1b]7777;open;{path}\x07");
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    }
}

/// Drop `..` lexically, as the shell's `cd` does, so the app sees the path
/// the user meant rather than one through a symlink's target.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences() {
        assert_eq!(osc("/a b/c;d", false), "\x1b]7777;open;/a b/c;d\x07");
        assert_eq!(osc("/x", true), "\x1bPtmux;\x1b\x1b]7777;open;/x\x07\x1b\\");
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
        assert!(sequence("/", false).unwrap_err().contains("folder"));
        assert!(sequence("/no/such/file", false).is_err());
    }
}
