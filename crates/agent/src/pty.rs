//! Terminals: the login shell on a pseudo-terminal, streamed as events.
//!
//! Each terminal has a reader thread that forwards output as it arrives and a
//! writer thread that feeds input in order, so a shell that is busy and not
//! reading never stalls the agent. When the agent exits, the masters close and
//! the kernel hangs up every shell, so nothing is left running.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use yonder_proto::{AgentMsg, Error, ErrorKind, Event, Op, Reply};

pub type Emit = Arc<dyn Fn(&AgentMsg) + Send + Sync>;

/// Output sent but not yet drawn by the app, beyond which reading pauses.
const MAX_UNACKED: u64 = 1 << 20;

#[derive(Default)]
struct FlowState {
    unacked: u64,
    closed: bool,
}

/// Lets the reader wait until the app catches up.
#[derive(Default)]
struct Flow {
    state: Mutex<FlowState>,
    changed: Condvar,
}

impl Flow {
    /// Wait until there is room for more output; false once closed.
    fn wait_for_room(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        while s.unacked > MAX_UNACKED && !s.closed {
            s = self.changed.wait(s).unwrap();
        }
        !s.closed
    }

    fn sent(&self, n: usize) {
        self.state.lock().unwrap().unacked += n as u64;
    }

    fn acked(&self, n: u64) {
        let mut s = self.state.lock().unwrap();
        s.unacked = s.unacked.saturating_sub(n);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

struct Handle {
    /// Kept for resizing.
    master: File,
    input: mpsc::Sender<Vec<u8>>,
    /// The shell, which leads its own session and process group.
    pid: i32,
    flow: Arc<Flow>,
}

pub struct Ptys {
    next: AtomicU64,
    open: Arc<Mutex<HashMap<u64, Handle>>>,
    emit: Emit,
}

impl Ptys {
    pub fn new(emit: Emit) -> Self {
        Ptys {
            next: AtomicU64::new(1),
            open: Arc::default(),
            emit,
        }
    }

    pub fn handle(&self, op: Op) -> Result<Reply, Error> {
        match op {
            Op::PtyOpen { cols, rows, cwd } => self.spawn(cols, rows, cwd),
            Op::PtyInput { pty, data } => {
                let open = self.open.lock().unwrap();
                let h = open.get(&pty).ok_or_else(|| gone(pty))?;
                h.input.send(data).map_err(|_| gone(pty))?;
                Ok(Reply::Done)
            }
            Op::PtyResize { pty, cols, rows } => {
                let open = self.open.lock().unwrap();
                let h = open.get(&pty).ok_or_else(|| gone(pty))?;
                set_size(&h.master, cols, rows)?;
                Ok(Reply::Done)
            }
            Op::PtyAck { pty, bytes } => {
                if let Some(h) = self.open.lock().unwrap().get(&pty) {
                    h.flow.acked(bytes);
                }
                Ok(Reply::Done)
            }
            Op::PtyClose { pty } => {
                if let Some(h) = self.open.lock().unwrap().remove(&pty) {
                    // Stop reading even if output is still unacknowledged;
                    // the reader then reports the exit.
                    h.flow.close();
                    unsafe { libc::kill(-h.pid, libc::SIGHUP) };
                }
                Ok(Reply::Done)
            }
            other => Err(Error::new(
                ErrorKind::Other,
                format!("not a terminal request: {other:?}"),
            )),
        }
    }

    fn spawn(&self, cols: u16, rows: u16, cwd: Option<String>) -> Result<Reply, Error> {
        let (master, slave) = open_pty(cols, rows)?;
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut cmd = Command::new(&shell);
        // A login shell, as a terminal ssh session would start: it reads
        // ~/.bash_profile or ~/.login, so modules and PATH match.
        cmd.arg("-l")
            .current_dir(cwd.unwrap_or(home))
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(|| {
                // New session, with the terminal as its controlling tty, so
                // job control and Ctrl-C work.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| {
            let message = format!("could not start {shell}: {e}");
            Error::new(Error::from(e).kind, message)
        })?;
        // Drop our copies of the slave side, or the reader never sees the end.
        drop(cmd);

        let pty = self.next.fetch_add(1, Ordering::Relaxed);
        let (input, queued) = mpsc::channel::<Vec<u8>>();
        let mut writer = master.try_clone()?;
        thread::spawn(move || {
            for data in queued {
                if writer.write_all(&data).is_err() {
                    break;
                }
            }
        });

        let mut reader = master.try_clone()?;
        let flow = Arc::new(Flow::default());
        // Registered before the reader starts, so a shell that exits at once
        // is still removed again.
        self.open.lock().unwrap().insert(
            pty,
            Handle {
                master,
                input,
                pid: child.id() as i32,
                flow: Arc::clone(&flow),
            },
        );
        let emit = Arc::clone(&self.emit);
        let open = Arc::clone(&self.open);
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while flow.wait_for_room() {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        flow.sent(n);
                        emit(&AgentMsg::Event(Event::PtyOutput {
                            pty,
                            data: buf[..n].to_vec(),
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // EIO: every process using the terminal has gone.
                    Err(_) => break,
                }
            }
            flow.close();
            let code = child.wait().ok().and_then(|s| s.code());
            open.lock().unwrap().remove(&pty);
            emit(&AgentMsg::Event(Event::PtyExit { pty, code }));
        });

        Ok(Reply::Pty { pty })
    }
}

fn gone(pty: u64) -> Error {
    Error::new(ErrorKind::NotFound, format!("terminal {pty} has exited"))
}

fn open_pty(cols: u16, rows: u16) -> io::Result<(File, OwnedFd)> {
    let ws = winsize(cols, rows);
    let (mut master, mut slave) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // Owned first, so an error below still closes both.
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Neither descriptor may leak into the shell or into later terminals:
    // a stray copy keeps a terminal open after its shell has gone.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn set_size(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    let ws = winsize(cols, rows);
    // The kernel sends SIGWINCH to the foreground job.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
