//! The host `local`: the real agent started without ssh, in a throwaway
//! `$HOME`. Its own test binary, since it changes the environment.

use std::sync::Arc;
use yonder_client::{connect, ConnectOptions, LogFn, LOCAL_HOST};
use yonder_proto::{Op, Reply};

#[tokio::test]
async fn local_host_runs_the_agent_without_ssh() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let agents = root.path().join("agents");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_yonder-agent"),
        agents.join(format!("yonder-agent-{}-macos", std::env::consts::ARCH)),
    )
    .unwrap();
    // SAFETY: the only test in this binary, before any other thread reads them.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("SHELL", "/bin/sh");
    }
    let mut opts = ConnectOptions::new(LOCAL_HOST, vec![agents]);
    // Never used: local connections do not run ssh.
    opts.ssh_program = "/nonexistent/ssh".into();
    let log: LogFn = Arc::new(|_| {});

    let conn = connect(&opts, Arc::clone(&log)).await.unwrap();
    assert_eq!(conn.info().home, home.to_string_lossy());
    std::fs::write(home.join("a.txt"), "hi").unwrap();
    match conn
        .call(Op::ListDir {
            path: home.to_string_lossy().into(),
        })
        .await
        .unwrap()
    {
        Reply::Entries(e) => assert!(e.iter().any(|e| e.name == "a.txt")),
        other => panic!("{other:?}"),
    }
    drop(conn);
    // The second connect reuses the installed copy.
    let _conn = connect(&opts, log).await.unwrap();
    assert_eq!(
        std::fs::read_dir(home.join(".cache/yonder"))
            .unwrap()
            .count(),
        1
    );
}
