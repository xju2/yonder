//! `yonder FILE...`: open files in the app from one of its terminals.
//!
//! The agent links itself as `yonder` on the terminal's `PATH` (see
//! `pty::command_dir`). Run under that name in a Yonder terminal, it prints an
//! escape sequence per file that the app's terminal catches. Anywhere else (an
//! OS terminal, ssh from one) it hands the path to a running agent through a
//! unix socket; that agent tells the app, which opens the file.

use crate::pty::Emit;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use yonder_proto::{AgentMsg, Event};

/// `/tmp/yonder-UID`, private to the user. Not `$HOME`: sockets do not work
/// on every shared filesystem, and `/tmp` is where a login node's agents are.
fn socket_dir() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/yonder-{uid}"));
    let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
    let m = fs::symlink_metadata(&dir).ok()?;
    (m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0).then_some(dir)
}

/// Listen for `yonder FILE` from outside the app's terminals, for as long as
/// the agent runs. Returns the socket's path so the caller can remove it.
pub fn listen(emit: Emit) -> Option<PathBuf> {
    let path = socket_dir()?.join(format!("{}.sock", std::process::id()));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_ok() {
                let p = line.trim_end_matches('\n');
                if p.starts_with('/') && !p.chars().any(char::is_control) {
                    emit(&AgentMsg::Event(Event::OpenFile { path: p.into() }));
                }
            }
        }
    });
    Some(path)
}

/// Send `path` to the newest live agent. ponytail: newest wins when one
/// host has several windows connected; target by window if that bites.
fn send(path: &str) -> Result<(), String> {
    let dir = socket_dir().ok_or("no private socket directory")?;
    let mut socks: Vec<_> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    socks.sort_by_key(|s| std::cmp::Reverse(s.0));
    for (_, sock) in socks {
        match UnixStream::connect(&sock) {
            Ok(mut s) => return writeln!(s, "{path}").map_err(|e| e.to_string()),
            Err(_) => {
                let _ = fs::remove_file(sock); // its agent is gone
            }
        }
    }
    Err("no Yonder window is connected to this machine".into())
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: yonder FILE...\nOpens files in the Yonder window this terminal belongs to."
        );
        return 2;
    }
    let tmux = std::env::var_os("TMUX").is_some();
    let in_app = std::env::var_os("YONDER_TERMINAL").is_some();
    let mut out = io::stdout().lock();
    let mut code = 0;
    for arg in args {
        let sent = checked(arg).and_then(|p| {
            if in_app {
                let _ = out.write_all(osc(&p, tmux).as_bytes());
                Ok(())
            } else {
                send(&p)
            }
        });
        match sent {
            Ok(()) => {}
            Err(e) => {
                eprintln!("yonder: {arg}: {e}");
                code = 1;
            }
        }
    }
    let _ = out.flush();
    code
}

fn checked(arg: &str) -> Result<String, String> {
    let path = normalize(&std::path::absolute(arg).map_err(|e| e.to_string())?);
    let meta = path.metadata().map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("is a folder; open files only".into());
    }
    let path = path.to_str().ok_or("not a UTF-8 path")?;
    if path.chars().any(char::is_control) {
        return Err("contains control characters".into());
    }
    Ok(path.to_string())
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
        assert!(checked("/").unwrap_err().contains("folder"));
        assert!(checked("/no/such/file").is_err());
    }
}
