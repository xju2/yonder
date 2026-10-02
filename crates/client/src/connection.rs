use crate::{Level, LogFn, LogLine};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use yonder_proto::{
    decode, encode, frame_len, AgentMsg, Error, ErrorKind, HelloInfo, Op, Reply, Request,
};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Reply, Error>>>>>;

/// A running agent. Requests may be issued concurrently from any task.
pub struct Connection {
    tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    pending: Pending,
    next_id: AtomicU64,
    closed: watch::Receiver<Option<String>>,
    kill: Arc<Notify>,
    pub(crate) info: OnceLock<HelloInfo>,
}

impl Connection {
    pub(crate) fn start<R>(
        mut child: Child,
        mut stdin: ChildStdin,
        mut stdout: R,
        stderr_tail: Arc<Mutex<VecDeque<String>>>,
        log: LogFn,
    ) -> Connection
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Pending = Arc::default();
        let (closed_tx, closed) = watch::channel(None);
        let kill = Arc::new(Notify::new());

        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin tells the agent to exit.
        });

        let reader_pending = Arc::clone(&pending);
        let reader_kill = Arc::clone(&kill);
        tokio::spawn(async move {
            let reason = tokio::select! {
                r = read_loop(&mut stdout, &reader_pending) => r,
                _ = reader_kill.notified() => {
                    let _ = child.start_kill();
                    "disconnected".to_string()
                }
            };
            let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            // Give the stderr reader a moment to collect ssh's last words.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let last = stderr_tail.lock().unwrap().back().cloned();
            let mut message = reason;
            if let Ok(Ok(status)) = status {
                if !status.success() {
                    message = format!("{message} (ssh exited with {status})");
                }
            }
            if let Some(last) = last {
                message = format!("{message}: {last}");
            }
            log(LogLine {
                level: Level::Warn,
                message: format!("Connection closed: {message}"),
            });
            let _ = closed_tx.send(Some(message));
            for (_, waiter) in reader_pending.lock().unwrap().drain() {
                let _ = waiter.send(Err(disconnected()));
            }
        });

        Connection {
            tx: Mutex::new(Some(tx)),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            kill,
            info: OnceLock::new(),
        }
    }

    /// What the agent reported when the connection was set up.
    pub fn info(&self) -> &HelloInfo {
        self.info
            .get()
            .expect("connect() sets info before returning")
    }

    pub async fn call(&self, op: Op) -> Result<Reply, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = encode(&Request { id, op }).map_err(Error::from)?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // The reader marks the connection closed before failing all pending
        // requests, so checking after inserting cannot miss a close.
        let sender = self.tx.lock().unwrap().clone();
        let sent = match sender {
            Some(s) if self.closed.borrow().is_none() => s.send(frame).is_ok(),
            _ => false,
        };
        if !sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(disconnected());
        }
        rx.await.unwrap_or_else(|_| Err(disconnected()))
    }

    /// Resolves to the reason once the connection is gone.
    pub async fn closed(&self) -> String {
        let mut rx = self.closed.clone();
        loop {
            if let Some(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return "disconnected".into();
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// Close stdin so the agent exits, and stop ssh.
    pub fn close(&self) {
        self.tx.lock().unwrap().take();
        self.kill.notify_one();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

fn disconnected() -> Error {
    Error::new(ErrorKind::Disconnected, "not connected to the remote")
}

async fn read_loop<R: AsyncBufRead + Unpin>(stdout: &mut R, pending: &Pending) -> String {
    loop {
        let mut prefix = [0u8; 4];
        if let Err(e) = stdout.read_exact(&mut prefix).await {
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                "the agent exited".into()
            } else {
                format!("read failed: {e}")
            };
        }
        let len = match frame_len(prefix) {
            Ok(len) => len,
            Err(e) => return e.to_string(),
        };
        let mut body = vec![0u8; len];
        if let Err(e) = stdout.read_exact(&mut body).await {
            return format!("read failed: {e}");
        }
        match decode::<AgentMsg>(&body) {
            Ok(AgentMsg::Response { id, result }) => {
                if let Some(waiter) = pending.lock().unwrap().remove(&id) {
                    let _ = waiter.send(result);
                }
            }
            Err(e) => return format!("bad message from agent: {e}"),
        }
    }
}
