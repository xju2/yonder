//! Runs the real bootstrap against the real agent. A stand-in for `ssh`
//! executes the remote command with `sh` locally, in a throwaway `$HOME`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use yonder_client::{connect, ConnectOptions, Level, LogFn, LogLine};
use yonder_proto::{ErrorKind, Event, Op, Reply};

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
                 for last; do :; done\nexport HOME='{}' SHELL=/bin/sh\nprintf '{rc_noise}'\nexec sh -c \"$last\"\n",
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
        .map(|d| d.flatten().filter(|e| e.file_name() != "bin").count())
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
    // ssh complaining during login, like a mistyped first password.
    let ssh = fx.root.path().join("ssh");
    let script = std::fs::read_to_string(&ssh).unwrap().replacen(
        "exec sh",
        // Then quiet, like real ssh once logged in.
        "echo 'Permission denied, please try again.' >&2\nexec 2>/dev/null\nexec sh",
        1,
    );
    std::fs::write(&ssh, script).unwrap();
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
    // What ssh said while logging in is not blamed for the disconnect.
    assert!(!reason.contains("Permission denied"), "{reason}");
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
    assert!(err.hint.unwrap().contains("Authentication failed"));
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

/// Collect terminal output until it contains `want`, or fail after a while.
async fn output_until(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    pty: u64,
    want: &str,
) -> String {
    let mut seen = String::new();
    let wait = async {
        while let Some(e) = events.recv().await {
            if let Event::PtyOutput { pty: p, data } = e {
                assert_eq!(p, pty);
                seen.push_str(&String::from_utf8_lossy(&data));
                if seen.contains(want) {
                    return;
                }
            }
        }
        panic!("events ended before {want:?} appeared in: {seen:?}");
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("no {want:?} in terminal output: {seen:?}"));
    seen
}

#[tokio::test]
async fn terminal_runs_commands_in_order() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    let mut events = conn.take_events().unwrap();
    assert!(conn.take_events().is_none());

    let cwd = fx.home().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let open = Op::PtyOpen {
        cols: 80,
        rows: 24,
        cwd: Some(cwd.to_string_lossy().into()),
    };
    let Ok(Reply::Pty { pty }) = conn.call(open).await else {
        panic!("no terminal")
    };

    // One keystroke per request, the way typing arrives.
    for b in "echo \"$((6*7)) in $(pwd)\"\n".bytes() {
        conn.send(Op::PtyInput { pty, data: vec![b] }).unwrap();
    }
    let out = output_until(&mut events, pty, "42 in ").await;
    assert!(out.contains(&format!("42 in {}", cwd.display())), "{out:?}");

    let resize = Op::PtyResize {
        pty,
        cols: 132,
        rows: 40,
    };
    assert_eq!(conn.call(resize).await.unwrap(), Reply::Done);
    conn.send(Op::PtyInput {
        pty,
        data: b"stty size\n".to_vec(),
    })
    .unwrap();
    output_until(&mut events, pty, "40 132").await;

    conn.send(Op::PtyInput {
        pty,
        data: b"exit 3\n".to_vec(),
    })
    .unwrap();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(Event::PtyExit { pty: p, code }) = events.recv().await {
                return (p, code);
            }
        }
    })
    .await
    .expect("no exit event");
    assert_eq!(exit, (pty, Some(3)));

    let input = Op::PtyInput {
        pty,
        data: b"x".to_vec(),
    };
    assert_eq!(
        conn.call(input).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[tokio::test]
async fn closing_a_terminal_hangs_it_up() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    let mut events = conn.take_events().unwrap();
    let open = Op::PtyOpen {
        cols: 80,
        rows: 24,
        cwd: None,
    };
    let Ok(Reply::Pty { pty }) = conn.call(open).await else {
        panic!("no terminal")
    };
    conn.send(Op::PtyInput {
        pty,
        data: b"echo ready\n".to_vec(),
    })
    .unwrap();
    output_until(&mut events, pty, "ready").await;

    assert_eq!(conn.call(Op::PtyClose { pty }).await.unwrap(), Reply::Done);
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(Event::PtyExit { pty: p, .. }) = events.recv().await {
                return p;
            }
        }
    })
    .await
    .expect("closing did not end the shell");
    assert_eq!(exited, pty);
}

#[tokio::test]
async fn terminal_output_waits_for_acknowledgement() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    let mut events = conn.take_events().unwrap();
    let open = Op::PtyOpen {
        cols: 80,
        rows: 24,
        cwd: None,
    };
    let Ok(Reply::Pty { pty }) = conn.call(open).await else {
        panic!("no terminal")
    };
    // An endless writer, like `yes` or `cat` of a huge log.
    conn.send(Op::PtyInput {
        pty,
        data: b"yes\n".to_vec(),
    })
    .unwrap();

    // Without acknowledgements, output stops a little past the 1 MiB window.
    let mut received = 0u64;
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
            Ok(Some(Event::PtyOutput { data, .. })) => received += data.len() as u64,
            Ok(_) => panic!("terminal ended"),
            Err(_) => break, // quiet: the agent stopped reading
        }
    }
    assert!(
        received > (1 << 20) && received < (2 << 20),
        "received {received} bytes"
    );

    // Acknowledging lets more through.
    conn.send(Op::PtyAck {
        pty,
        bytes: received,
    })
    .unwrap();
    let more = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await;
    assert!(matches!(more, Ok(Some(Event::PtyOutput { .. }))));

    // Ctrl-C still reaches the program, and closing ends the terminal.
    conn.send(Op::PtyInput { pty, data: vec![3] }).unwrap();
    assert_eq!(conn.call(Op::PtyClose { pty }).await.unwrap(), Reply::Done);
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(Event::PtyExit { pty: p, .. }) => return p,
                Some(_) => {}
                None => panic!("events ended"),
            }
        }
    })
    .await
    .expect("terminal did not exit");
    assert_eq!(exited, pty);
}

#[tokio::test]
async fn git_status_history_and_contents() {
    use yonder_client::git;
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();

    let repo = fx.home().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let sh = |cmd: &str| {
        let ok = std::process::Command::new("sh")
            .args(["-c", cmd])
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.org")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.org")
            .status()
            .unwrap()
            .success();
        assert!(ok, "{cmd}");
    };
    let root = repo.to_string_lossy().to_string();
    let sub = repo.join("src").to_string_lossy().to_string();

    // Not a repository yet.
    assert_eq!(git::repo_root(&conn, &root).await.unwrap(), None);

    sh(
        "git init -q -b main && printf 'one\\n' > a.txt && printf 'old\\n' > src/old.rs \
        && git add . && git commit -qm 'First commit'",
    );
    sh("git mv src/old.rs src/new.rs && printf 'one\\ntwo\\n' > a.txt && git commit -qam 'Second: rename'");
    sh(
        "printf 'one\\ntwo\\nthree\\n' > a.txt && printf 'x\\n' > untracked.txt \
        && mkdir -p newdir/deep && printf 'y\\n' > newdir/deep/file.txt",
    );

    // Found from a subfolder too.
    let top = git::repo_root(&conn, &sub).await.unwrap().unwrap();
    assert_eq!(
        std::fs::canonicalize(&top).unwrap(),
        std::fs::canonicalize(&repo).unwrap()
    );

    let st = git::status(&conn, &top).await.unwrap();
    assert_eq!(st.branch.as_deref(), Some("main"));
    let files: Vec<_> = st
        .files
        .iter()
        .map(|c| (c.status, c.path.as_str()))
        .collect();
    // Untracked folders are listed file by file, so every row opens.
    assert_eq!(
        files,
        vec![
            ('M', "a.txt"),
            ('?', "newdir/deep/file.txt"),
            ('?', "untracked.txt")
        ]
    );

    let log = git::log(&conn, &top, 0, 10).await.unwrap();
    let subjects: Vec<_> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["Second: rename", "First commit"]);
    assert_eq!(log[0].author, "Ada");
    assert_eq!(git::log(&conn, &top, 1, 10).await.unwrap().len(), 1);

    let changed = git::commit_files(&conn, &top, &log[0].hash).await.unwrap();
    let changed: Vec<_> = changed
        .iter()
        .map(|c| (c.status, c.path.as_str(), c.old_path.as_deref()))
        .collect();
    assert_eq!(
        changed,
        vec![
            ('M', "a.txt", None),
            ('R', "src/new.rs", Some("src/old.rs"))
        ]
    );
    // The root commit lists everything as added.
    let first = git::commit_files(&conn, &top, &log[1].hash).await.unwrap();
    assert!(first.iter().all(|c| c.status == 'A'));

    assert_eq!(
        git::file_at(&conn, &top, "HEAD", "a.txt")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"one\ntwo\n"[..])
    );
    let parent = format!("{}^", log[1].hash);
    assert_eq!(
        git::file_at(&conn, &top, &parent, "a.txt").await.unwrap(),
        None
    );
    assert_eq!(
        git::file_at(&conn, &top, "HEAD", "missing.txt")
            .await
            .unwrap(),
        None
    );

    // A symbolic link reads as its stored target, the way git stores it.
    sh("ln -s a.txt link");
    let link = Op::ReadLink {
        path: format!("{root}/link"),
    };
    assert_eq!(
        conn.call(link).await.unwrap(),
        Reply::Path {
            path: "a.txt".into(),
            is_dir: false
        }
    );

    // A committed file past the diff limit is reported as too large.
    sh("head -c 17000000 /dev/zero > big.bin && git add big.bin && git commit -qm big");
    let err = git::file_at(&conn, &top, "HEAD", "big.bin")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::TooLarge, "{err}");
}

/// Bytes from stream `id` until `want` appears, and whether it then ended.
async fn stream_until(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    id: u64,
    want: &str,
) -> String {
    let mut seen = String::new();
    let wait = async {
        while let Some(e) = events.recv().await {
            if let Event::StreamOutput { id: i, data, .. } = e {
                assert_eq!(i, id);
                seen.push_str(&String::from_utf8_lossy(&data));
                if seen.contains(want) {
                    return;
                }
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("no {want:?} in stream output: {seen:?}"));
    seen
}

async fn stream_exit(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    id: u64,
) -> Option<i32> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(Event::StreamExit { id: i, code }) if i == id => return code,
                Some(_) => {}
                None => panic!("events ended"),
            }
        }
    })
    .await
    .expect("no exit event")
}

#[tokio::test]
async fn process_and_tcp_streams() {
    let fx = Fixture::new("");
    let (_, sink) = collect_log();
    let conn = connect(&fx.options(), sink).await.unwrap();
    let mut events = conn.take_events().unwrap();
    let cwd = fx.home().to_string_lossy().to_string();

    // A process: stdin to stdout in order, stderr apart, and its exit code.
    let open = Op::ProcOpen {
        id: 7,
        cwd: cwd.clone(),
        script: "echo \"in $(pwd)\" >&2; while read l; do echo \"got $l\"; done; exit 4".into(),
    };
    assert_eq!(conn.call(open).await.unwrap(), Reply::Done);
    for l in ["a\n", "b\n"] {
        conn.send(Op::StreamInput {
            id: 7,
            data: l.into(),
        })
        .unwrap();
    }
    let out = stream_until(&mut events, 7, "got b").await;
    assert!(out.contains(&format!("in {cwd}")), "{out:?}");
    assert!(out.contains("got a\ngot b"), "{out:?}");

    // Closing ends the process group.
    assert_eq!(
        conn.call(Op::StreamClose { id: 7 }).await.unwrap(),
        Reply::Done
    );
    assert_eq!(stream_exit(&mut events, 7).await, None);

    // A socket on the remote's loopback.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        s.write_all(&[b"echo:", &buf[..]].concat()).unwrap();
    });
    assert_eq!(
        conn.call(Op::TcpOpen { id: 8, port }).await.unwrap(),
        Reply::Done
    );
    conn.send(Op::StreamInput {
        id: 8,
        data: b"hello".to_vec(),
    })
    .unwrap();
    stream_until(&mut events, 8, "echo:hello").await;
    server.join().unwrap();
    assert_eq!(stream_exit(&mut events, 8).await, None);

    let refused = Op::TcpOpen { id: 9, port };
    assert!(conn.call(refused).await.is_err());
}
