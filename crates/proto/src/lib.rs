//! Wire protocol between the Yonder app and `yonder-agent`.
//!
//! The agent talks over its stdin/stdout, which the app reaches through a
//! single `ssh` session. After the agent starts it writes [`MAGIC`]; anything
//! the remote login shell printed before that (rc-file noise) is skipped.
//! From then on both directions carry frames: a big-endian `u32` length
//! followed by that many bytes of MessagePack.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Written by the agent once, before its first frame.
pub const MAGIC: &[u8] = b"\0YONDER-AGENT-1\n";

/// Bumped whenever a message changes shape. The app refuses agents that differ.
pub const PROTOCOL_VERSION: u32 = 2;

/// Upper bound for one frame, so a corrupt length cannot exhaust memory.
pub const MAX_FRAME: usize = 256 << 20;

/// App -> agent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub op: Op,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Op {
    Hello,
    /// Expand `~`, make relative paths relative to `$HOME`, and canonicalize.
    Resolve {
        path: String,
    },
    ListDir {
        path: String,
    },
    /// Size and modification time, following symlinks. Lets viewers notice a
    /// regenerated plot without downloading it again.
    Stat {
        path: String,
    },
    /// Fails with [`ErrorKind::TooLarge`] if the file exceeds `max_bytes`.
    ReadFile {
        path: String,
        max_bytes: u64,
    },
    /// With `expected_hash`, the write only happens if the file on disk still
    /// hashes to it; otherwise the reply is [`ErrorKind::Conflict`].
    /// `None` means "create or overwrite unconditionally".
    ///
    /// The check runs immediately before the new content is committed, but
    /// POSIX offers no compare-and-swap for files, so a write by another
    /// process in that last instant can still be overwritten. It catches the
    /// common case: a file changed minutes ago by a job or another editor.
    WriteFile {
        path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        expected_hash: Option<u64>,
    },
}

/// Agent -> app.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AgentMsg {
    Response {
        id: u64,
        result: Result<Reply, Error>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Reply {
    Hello(HelloInfo),
    Path {
        path: String,
        is_dir: bool,
    },
    Entries(Vec<Entry>),
    Stat(FileStat),
    File {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        hash: u64,
        stat: FileStat,
    },
    Written {
        hash: u64,
        stat: FileStat,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HelloInfo {
    pub agent_version: String,
    pub protocol: u32,
    pub hostname: String,
    pub home: String,
    pub pid: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symlink whose target is missing or unreadable.
    BrokenLink,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub symlink: bool,
    pub stat: FileStat,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_ns: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    IsDirectory,
    NotDirectory,
    TooLarge,
    /// The file changed on disk since the app read it.
    Conflict,
    /// Raised by the app, not the agent: the ssh session is gone.
    Disconnected,
    Other,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        let kind = match e.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::IsADirectory => ErrorKind::IsDirectory,
            io::ErrorKind::NotADirectory => ErrorKind::NotDirectory,
            _ => ErrorKind::Other,
        };
        Error::new(kind, e.to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// FNV-1a, 64 bit. Used to detect that a file changed between read and save;
/// mtime alone is not enough on filesystems with one-second resolution.
#[derive(Clone, Copy, Debug)]
pub struct ContentHasher(u64);

impl Default for ContentHasher {
    fn default() -> Self {
        ContentHasher(0xcbf2_9ce4_8422_2325)
    }
}

impl ContentHasher {
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

pub fn content_hash(data: &[u8]) -> u64 {
    let mut h = ContentHasher::default();
    h.update(data);
    h.finish()
}

/// Serialize `msg` into a complete frame (length prefix included).
pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = rmp_serde::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {} bytes exceeds the limit", body.len()),
        ));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Deserialize a frame body (without the length prefix).
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    rmp_serde::from_slice(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Validate a length prefix.
pub fn frame_len(prefix: [u8; 4]) -> io::Result<usize> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds the limit; the stream is corrupt"),
        ));
    }
    Ok(len)
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)?;
    w.flush()
}

/// Read one frame. `Ok(None)` means the stream ended cleanly between frames.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut prefix = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut prefix[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => got += n,
        }
    }
    let mut body = vec![0u8; frame_len(prefix)?];
    r.read_exact(&mut body)?;
    decode(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request {
            id: 7,
            op: Op::WriteFile {
                path: "/tmp/a b".into(),
                data: vec![0, 1, 2, 255],
                expected_hash: Some(42),
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame::<_, Request>(&mut r).unwrap(), Some(req));
        assert_eq!(read_frame::<_, Request>(&mut r).unwrap(), None);
    }

    #[test]
    fn response_roundtrip() {
        let msg = AgentMsg::Response {
            id: 1,
            result: Err(Error::new(ErrorKind::Conflict, "changed")),
        };
        let frame = encode(&msg).unwrap();
        assert_eq!(decode::<AgentMsg>(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let frame = encode(&Request {
            id: 1,
            op: Op::Hello,
        })
        .unwrap();
        let mut r = &frame[..frame.len() - 1];
        assert!(read_frame::<_, Request>(&mut r).is_err());
    }

    #[test]
    fn oversized_length_is_rejected() {
        assert!(frame_len(u32::MAX.to_be_bytes()).is_err());
    }

    #[test]
    fn incremental_hash_matches() {
        let mut h = ContentHasher::default();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finish(), content_hash(b"hello world"));
    }

    #[test]
    fn hash_distinguishes_content() {
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
        assert_eq!(content_hash(b""), 0xcbf2_9ce4_8422_2325);
    }
}
