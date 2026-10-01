//! `yonder-agent`: serves file operations over stdin/stdout.
//!
//! The app starts it through `ssh`, so its lifetime is the ssh session's:
//! when stdin closes the agent exits. It keeps no state on disk and takes no
//! locks, which matters on shared filesystems such as Lustre or NFS.

mod ops;

use std::io::{self, BufReader, BufWriter, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use yonder_proto::{read_frame, write_frame, AgentMsg, Request, MAGIC};

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
    let out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    {
        let mut w = out.lock().unwrap();
        w.write_all(MAGIC)?;
        w.flush()?;
    }
    let mut input = BufReader::new(io::stdin());
    while let Some(req) = read_frame::<_, Request>(&mut input)? {
        // One thread per request: a slow stat on a busy metadata server must
        // not hold up an unrelated read.
        let out = Arc::clone(&out);
        thread::spawn(move || {
            let msg = AgentMsg::Response {
                id: req.id,
                result: ops::handle(req.op),
            };
            let mut w = out.lock().unwrap();
            if let Err(e) = write_frame(&mut *w, &msg) {
                eprintln!("yonder-agent: write failed: {e}");
                std::process::exit(1);
            }
        });
    }
    Ok(())
}
