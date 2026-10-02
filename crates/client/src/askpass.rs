//! Answering ssh's questions (passwords, one-time codes, unknown host keys)
//! from the app's window.
//!
//! ssh runs without a terminal, so it asks through `SSH_ASKPASS`: it starts
//! that program with the question as its argument and reads the answer from
//! its stdout. Yonder points `SSH_ASKPASS` at its own executable; started
//! that way, [`client`] forwards the question over a Unix socket to the
//! running app, which [`Server`] passes to a handler (the UI) and sends back.
//! The socket lives in a directory only the user can open.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// Set in the environment of the ssh process; its presence also tells the
/// executable to act as the askpass helper.
pub const SOCKET_ENV: &str = "YONDER_ASKPASS_SOCKET";

/// Answers a question, or `None` to cancel. Called on a background thread;
/// it may block until the person answers.
pub type Handler = Arc<dyn Fn(String) -> Option<String> + Send + Sync>;

/// What ssh needs to ask through the app.
#[derive(Clone, Debug)]
pub struct Askpass {
    /// The program ssh starts: this executable.
    pub program: PathBuf,
    pub socket: PathBuf,
}

/// Listens for questions until dropped.
pub struct Server {
    dir: PathBuf,
    socket: PathBuf,
}

impl Server {
    pub fn start(handler: Handler) -> io::Result<Server> {
        let dir = std::env::temp_dir().join(format!("yonder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let socket = dir.join("askpass.sock");
        let listener = UnixListener::bind(&socket)?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    let _ = answer_one(stream, &handler);
                });
            }
        });
        Ok(Server { dir, socket })
    }

    pub fn askpass(&self) -> io::Result<Askpass> {
        Ok(Askpass {
            program: std::env::current_exe()?,
            socket: self.socket.clone(),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// Messages are length-prefixed. The answer starts with one byte: 1 for an
// answer, 0 for cancelled.

fn write_blob(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)
}

fn read_blob(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too long",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn answer_one(mut stream: UnixStream, handler: &Handler) -> io::Result<()> {
    let prompt = String::from_utf8_lossy(&read_blob(&mut stream)?).into_owned();
    match handler(prompt) {
        Some(answer) => {
            stream.write_all(&[1])?;
            write_blob(&mut stream, answer.as_bytes())
        }
        None => stream.write_all(&[0]),
    }
}

/// The askpass helper: ask the app over `socket`, print the answer for ssh.
/// Returns the process exit code: 0 answered, 1 cancelled or failed.
pub fn client(socket: &Path, prompt: &str) -> i32 {
    let ask = || -> io::Result<Option<Vec<u8>>> {
        let mut stream = UnixStream::connect(socket)?;
        write_blob(&mut stream, prompt.as_bytes())?;
        let mut kind = [0u8; 1];
        stream.read_exact(&mut kind)?;
        if kind[0] == 1 {
            Ok(Some(read_blob(&mut stream)?))
        } else {
            Ok(None)
        }
    };
    match ask() {
        Ok(Some(answer)) => {
            let mut out = io::stdout();
            let _ = out.write_all(&answer);
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            0
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn question_and_answer_roundtrip() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let server = Server::start(Arc::new(move |prompt: String| {
            seen.lock().unwrap().push(prompt.clone());
            (!prompt.contains("cancel")).then(|| "s3cret".to_string())
        }))
        .unwrap();
        let socket = server.askpass().unwrap().socket;
        // Only the user may enter the socket's directory.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        assert_eq!(client(&socket, "Password: "), 0);
        assert_eq!(client(&socket, "please cancel"), 1);
        assert_eq!(*asked.lock().unwrap(), vec!["Password: ", "please cancel"]);

        let dir = socket.parent().unwrap().to_path_buf();
        drop(server);
        assert!(!dir.exists());
    }
}
