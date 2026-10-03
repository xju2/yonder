//! Streams: a process with pipes (language servers, Jupyter) or a TCP
//! connection to a port on the remote's loopback (Jupyter's web page).
//!
//! Like terminals, each has a writer thread fed in order and reader threads
//! that forward what arrives. Processes lead their own process group, which
//! is sent SIGTERM when the stream is closed or the agent exits; on Linux
//! they also get it if the agent dies without cleaning up.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use yonder_proto::{AgentMsg, Error, ErrorKind, Event, Op, Reply};

use crate::pty::Emit;

// ponytail: no flow control, unlike terminals; language servers and Jupyter
// send bounded replies. Add acks like PtyAck if a stream ever floods.

enum Kill {
    Group(i32),
    Socket(TcpStream),
}

struct Handle {
    input: mpsc::Sender<Vec<u8>>,
    kill: Kill,
}

impl Handle {
    fn kill(&self) {
        match &self.kill {
            Kill::Group(pid) => unsafe {
                libc::kill(-pid, libc::SIGTERM);
            },
            Kill::Socket(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

pub struct Streams {
    open: Arc<Mutex<HashMap<u64, Handle>>>,
    emit: Emit,
}

impl Streams {
    pub fn new(emit: Emit) -> Self {
        Streams {
            open: Arc::default(),
            emit,
        }
    }

    pub fn handle(&self, op: Op) -> Result<Reply, Error> {
        match op {
            Op::ProcOpen { id, cwd, script } => self.spawn(id, &cwd, &script),
            Op::TcpOpen { id, port } => self.connect(id, port),
            Op::StreamInput { id, data } => {
                let open = self.open.lock().unwrap();
                let h = open.get(&id).ok_or_else(|| gone(id))?;
                h.input.send(data).map_err(|_| gone(id))?;
                Ok(Reply::Done)
            }
            Op::StreamClose { id } => {
                if let Some(h) = self.open.lock().unwrap().remove(&id) {
                    h.kill();
                }
                Ok(Reply::Done)
            }
            other => Err(Error::new(
                ErrorKind::Other,
                format!("not a stream request: {other:?}"),
            )),
        }
    }

    /// End every process: the agent is exiting.
    pub fn close_all(&self) {
        for (_, h) in self.open.lock().unwrap().drain() {
            h.kill();
        }
    }

    fn spawn(&self, id: u64, cwd: &str, script: &str) -> Result<Reply, Error> {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = Command::new(&shell);
        // The login shell sets up PATH and modules; the script itself runs
        // in sh whatever the login shell is (csh and fish included). The cd
        // comes after the login files, which sometimes change directory.
        cmd.args([
            "-l",
            "-c",
            r#"cd "$YONDER_CWD" && exec /bin/sh -c "$YONDER_SCRIPT""#,
        ])
        .env("YONDER_CWD", cwd)
        .env("YONDER_SCRIPT", script)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| {
            let message = format!("could not start {shell}: {e}");
            Error::new(Error::from(e).kind, message)
        })?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let input = writer(stdin);
        self.open.lock().unwrap().insert(
            id,
            Handle {
                input,
                kill: Kill::Group(child.id() as i32),
            },
        );
        let err_reader = reader(Arc::clone(&self.emit), id, true, stderr);
        let emit = Arc::clone(&self.emit);
        let open = Arc::clone(&self.open);
        thread::spawn(move || {
            pump(&emit, id, false, stdout);
            let _ = err_reader.join();
            let code = child.wait().ok().and_then(|s| s.code());
            open.lock().unwrap().remove(&id);
            emit(&AgentMsg::Event(Event::StreamExit { id, code }));
        });
        Ok(Reply::Done)
    }

    fn connect(&self, id: u64, port: u16) -> Result<Reply, Error> {
        let sock = TcpStream::connect(("127.0.0.1", port))?;
        let _ = sock.set_nodelay(true);
        let input = writer(sock.try_clone()?);
        let read = sock.try_clone()?;
        self.open.lock().unwrap().insert(
            id,
            Handle {
                input,
                kill: Kill::Socket(sock),
            },
        );
        let emit = Arc::clone(&self.emit);
        let open = Arc::clone(&self.open);
        thread::spawn(move || {
            pump(&emit, id, false, read);
            open.lock().unwrap().remove(&id);
            emit(&AgentMsg::Event(Event::StreamExit { id, code: None }));
        });
        Ok(Reply::Done)
    }
}

fn writer<W: Write + Send + 'static>(mut w: W) -> mpsc::Sender<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        for data in rx {
            if w.write_all(&data).and_then(|_| w.flush()).is_err() {
                break;
            }
        }
    });
    tx
}

fn reader<R: Read + Send + 'static>(
    emit: Emit,
    id: u64,
    stderr: bool,
    r: R,
) -> thread::JoinHandle<()> {
    thread::spawn(move || pump(&emit, id, stderr, r))
}

fn pump<R: Read>(emit: &Emit, id: u64, stderr: bool, mut r: R) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => emit(&AgentMsg::Event(Event::StreamOutput {
                id,
                stderr,
                data: buf[..n].to_vec(),
            })),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn gone(id: u64) -> Error {
    Error::new(ErrorKind::NotFound, format!("stream {id} has ended"))
}
