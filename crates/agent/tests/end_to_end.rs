//! Runs the real bootstrap against the real agent. A stand-in for `ssh`
//! executes the remote command with `sh` locally, in a throwaway `$HOME`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use yonder_client::{connect, ConnectOptions, Level, LogFn, LogLine};
use yonder_proto::{ErrorKind, Op, Reply};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// `rc_noise` is printed on stdout before the command runs, the way a
    /// chatty login file would.
    fn new(rc_noise: &str) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let agents = root.path().join("agents");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::copy(
            env!("CARGO_BIN_EXE_yonder-agent"),
            agents.join(format!("yonder-agent-{}-linux", std::env::consts::ARCH)),
        )
        .unwrap();
        let fake_ssh = root.path().join("ssh");
        std::fs::write(
            &fake_ssh,
            format!(
                "#!/bin/sh\n# Drop the options and host; run the command the way sshd would.\n\
                 for last; do :; done\nexport HOME='{}'\nprintf '{rc_noise}'\nexec sh -c \"$last\"\n",
                home.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fixture { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn options(&self) -> ConnectOptions {
        let mut o = ConnectOptions::new("testhost", vec![self.root.path().join("agents")]);
        o.ssh_program = self.root.path().join("ssh");
        o
    }
}

fn collect_log() -> (Arc<Mutex<Vec<LogLine>>>, LogFn) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    (lines, Arc::new(move |l| sink.lock().unwrap().push(l)))
}

fn logged(log: &Mutex<Vec<LogLine>>, pred: impl Fn(&LogLine) -> bool) -> bool {
    log.lock().unwrap().iter().any(pred)
}

fn installed_agents(home: &Path) -> usize {
    std::fs::read_dir(home.join(".cache/yonder"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[tokio::test]
async fn installs_once_then_reuses() {
    let fx = Fixture::new("Welcome to the cluster\\nno newline here");

    let (log, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    assert_eq!(conn.info().home, fx.home().to_string_lossy());
    assert!(logged(&log, |l| l.message.starts_with("Installing agent")));
    assert!(logged(&log, |l| l.level == Level::Remote
        && l.message.contains("Welcome")));
    assert_eq!(installed_agents(&fx.home()), 1);
    drop(conn);

    let (log, sink) = collect_log();
    let _conn = connect(&fx.options(), sink).await.unwrap();
    assert!(!logged(&log, |l| l.message.starts_with("Installing agent")));
    assert_eq!(installed_agents(&fx.home()), 1);
}

#[tokio::test]
async fn noise_mentioning_a_marker_is_ignored() {
    let fx = Fixture::new("motd: see YONDER-HELLO Plan9 mips for details\\n");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    assert_eq!(conn.info().home, fx.home().to_string_lossy());
}

#[tokio::test]
async fn file_operations_over_the_connection() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = Arc::new(connect(&fx.options(), sink).await.unwrap());

    let Ok(Reply::Path {
        path: root,
        is_dir: true,
    }) = conn.call(Op::Resolve { path: "~".into() }).await
    else {
        panic!("home did not resolve to a directory")
    };
    let file = format!("{root}/notes.txt");
    let write = Op::WriteFile {
        path: file.clone(),
        data: b"one".to_vec(),
        expected_hash: None,
    };
    let Ok(Reply::Written { hash, .. }) = conn.call(write).await else {
        panic!("write failed")
    };

    // Many requests in flight at once each get their own answer.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let conn = Arc::clone(&conn);
        let op = Op::ReadFile {
            path: file.clone(),
            max_bytes: 1024,
        };
        tasks.spawn(async move { conn.call(op).await });
    }
    while let Some(r) = tasks.join_next().await {
        let Ok(Reply::File { data, hash: h, .. }) = r.unwrap() else {
            panic!("read failed")
        };
        assert_eq!((data.as_slice(), h), (&b"one"[..], hash));
    }

    std::fs::write(&file, "changed by someone else").unwrap();
    let write = Op::WriteFile {
        path: file.clone(),
        data: b"mine".to_vec(),
        expected_hash: Some(hash),
    };
    assert_eq!(
        conn.call(write).await.unwrap_err().kind,
        ErrorKind::Conflict
    );

    let Ok(Reply::Entries(entries)) = conn.call(Op::ListDir { path: root }).await else {
        panic!("list failed")
    };
    assert!(entries.iter().any(|e| e.name == "notes.txt"));

    conn.close();
    assert!(!conn.closed().await.is_empty());
    assert_eq!(
        conn.call(Op::Hello).await.unwrap_err().kind,
        ErrorKind::Disconnected
    );
}

#[tokio::test]
async fn agent_exit_is_noticed() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    let pid = conn.info().pid;
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let reason = tokio::time::timeout(std::time::Duration::from_secs(10), conn.closed())
        .await
        .expect("close was not noticed");
    assert!(reason.contains("agent exited"), "{reason}");
}

#[tokio::test]
async fn reports_ssh_failure_with_hint() {
    let fx = Fixture::new("");
    std::fs::write(
        fx.root.path().join("ssh"),
        "#!/bin/sh\necho 'user@host: Permission denied (publickey).' >&2\nexit 255\n",
    )
    .unwrap();
    let (_, sink) = collect_log();
    let err = connect(&fx.options(), sink).await.err().unwrap();
    assert!(err.message.contains("Permission denied"), "{err}");
    assert!(err.hint.unwrap().contains("BatchMode"));
}

#[tokio::test]
async fn reports_failed_install() {
    let fx = Fixture::new("");
    // A file where the cache directory should be makes `mkdir -p` fail.
    std::fs::write(fx.home().join(".cache"), "").unwrap();
    let (_, sink) = collect_log();
    let err = connect(&fx.options(), sink).await.err().unwrap();
    assert!(err.step.starts_with("Installing agent"), "{err}");
    assert!(err.message.contains("installing the agent failed"), "{err}");
}
