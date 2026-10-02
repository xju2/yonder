//! `yonder-agent`: serves file operations and terminals over stdin/stdout.
//!
//! The app starts it through `ssh`, so its lifetime is the ssh session's:
//! when stdin closes the agent exits, and its terminals hang up. It keeps no
//! state on disk and takes no locks, which matters on shared filesystems such
//! as Lustre or NFS.

mod ops;
mod pty;

use std::io::{self, BufReader, BufWriter, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use yonder_proto::{read_frame, write_frame, AgentMsg, Op, Request, MAGIC, NO_REPLY};

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("yonder-agent {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = serve() {
        eprintln!("yonder-agent: {e}");
        std::process::exit(1);
    }
}

fn serve() -> io::Result<()> {
    let out = Mutex::new(BufWriter::new(io::stdout()));
    {
        let mut w = out.lock().unwrap();
        w.write_all(MAGIC)?;
        w.flush()?;
    }
    let emit: pty::Emit = Arc::new(move |msg: &AgentMsg| {
        let mut w = out.lock().unwrap();
        if let Err(e) = write_frame(&mut *w, msg) {
            eprintln!("yonder-agent: write failed: {e}");
            std::process::exit(1);
        }
    });
    let ptys = pty::Ptys::new(Arc::clone(&emit));

    let mut input = BufReader::new(io::stdin());
    while let Some(Request { id, op }) = read_frame::<_, Request>(&mut input)? {
        let reply = |result| {
            if id != NO_REPLY {
                emit(&AgentMsg::Response { id, result });
            }
        };
        match op {
            // Terminal requests are quick and must keep their order:
            // keystrokes typed in sequence arrive in sequence.
            Op::PtyOpen { .. }
            | Op::PtyInput { .. }
            | Op::PtyResize { .. }
            | Op::PtyAck { .. }
            | Op::PtyClose { .. } => reply(ptys.handle(op)),
            // One thread per file request: a slow stat on a busy metadata
            // server must not hold up an unrelated read.
            op => {
                let emit = Arc::clone(&emit);
                thread::spawn(move || {
                    let result = ops::handle(op);
                    if id != NO_REPLY {
                        emit(&AgentMsg::Response { id, result });
                    }
                });
            }
        }
    }
    Ok(())
}
