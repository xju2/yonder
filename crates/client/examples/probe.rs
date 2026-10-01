//! Connect to a host without the GUI and list a directory. Handy for
//! debugging a connection: every step is printed.
//!
//!     cargo run -p yonder-client --example probe -- <host> [path]

use std::sync::Arc;
use yonder_client::{connect, ConnectOptions};
use yonder_proto::{Op, Reply};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: probe <host> [path]");
    let path = args.next().unwrap_or_else(|| "~".into());
    let agent_dirs = std::env::var_os("YONDER_AGENT_DIR")
        .map(|d| vec![d.into()])
        .unwrap_or_else(|| vec![concat!(env!("CARGO_MANIFEST_DIR"), "/../../target").into()]);
    let log = Arc::new(|l: yonder_client::LogLine| eprintln!("[{:?}] {}", l.level, l.message));
    let started = std::time::Instant::now();
    let conn = match connect(&ConnectOptions::new(host, agent_dirs), log).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("connected in {:?}", started.elapsed());
    let Ok(Reply::Path { path, .. }) = conn.call(Op::Resolve { path }).await else {
        eprintln!("cannot resolve path");
        std::process::exit(1);
    };
    let t = std::time::Instant::now();
    match conn.call(Op::ListDir { path: path.clone() }).await {
        Ok(Reply::Entries(entries)) => {
            for e in &entries {
                println!("{:?}\t{}", e.kind, e.name);
            }
            eprintln!(
                "listed {} entries of {path} in {:?}",
                entries.len(),
                t.elapsed()
            );
        }
        other => eprintln!("{other:?}"),
    }
}
