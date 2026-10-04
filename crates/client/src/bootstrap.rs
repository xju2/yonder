use crate::{Connection, Level, LogFn, LogLine};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use yonder_proto::{Op, Reply, MAGIC, PROTOCOL_VERSION};

/// Runs on the remote through the user's login shell, so it must be one line
/// of plain `sh` that also survives being quoted by csh or fish: no single
/// quotes, no `!`, no newlines.
///
/// It prints the OS and CPU, reads back `<agent hash> <size>`, and either
/// starts the cached agent or receives `<size>` bytes, installs them with an
/// atomic rename, and starts that. The leading `echo` puts the first marker on
/// its own line even if a login file printed something without a newline.
const REMOTE_SCRIPT: &str = "echo; echo \"YONDER-HELLO $(uname -s) $(uname -m)\"; \
read h n; d=\"$HOME/.cache/yonder\"; p=\"$d/agent-$h\"; \
if [ -x \"$p\" ]; then echo YONDER-START; exec \"$p\"; fi; \
echo YONDER-UPLOAD; t=\"$p.tmp.$$\"; \
mkdir -p \"$d\" && head -c \"$n\" > \"$t\" && chmod 755 \"$t\" && mv -f \"$t\" \"$p\" \
&& echo YONDER-START && exec \"$p\"; \
rm -f \"$t\"; echo YONDER-FAILED; exit 1";

pub struct ConnectOptions {
    /// Anything `ssh` accepts as a destination, typically a `Host` alias.
    pub host: String,
    /// The ssh client to run; `ssh` from `PATH` unless overridden.
    pub ssh_program: PathBuf,
    /// Where to look for agent builds; see [`find_agent`].
    pub agent_dirs: Vec<PathBuf>,
    /// Ask passwords, one-time codes and host-key questions through the
    /// app. Without it ssh runs in batch mode and such prompts fail.
    pub askpass: Option<crate::askpass::Askpass>,
}

impl ConnectOptions {
    pub fn new(host: impl Into<String>, agent_dirs: Vec<PathBuf>) -> Self {
        ConnectOptions {
            host: host.into(),
            ssh_program: std::env::var_os("YONDER_SSH")
                .map(PathBuf::from)
                .unwrap_or_else(|| "ssh".into()),
            agent_dirs,
            askpass: None,
        }
    }
}

/// Why connecting failed, phrased for the person at the keyboard.
#[derive(Debug, Clone)]
pub struct ConnectError {
    /// The step that failed, e.g. "Starting ssh".
    pub step: String,
    pub message: String,
    /// What to try next, when the failure is a recognised one.
    pub hint: Option<String>,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n{hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConnectError {}

/// CPU architectures (as `uname -m` prints them) Yonder builds agents for.
/// The value comes from the remote, so only these are ever used in a path.
pub const SUPPORTED_ARCHES: &[&str] = &["x86_64", "aarch64"];

/// Look for the agent build for `arch` (as printed by `uname -m`) in `dirs`.
/// Accepts both the packaged name `yonder-agent-<arch>-linux` and a Cargo
/// target directory layout, `<arch>-unknown-linux-musl/release/yonder-agent`.
pub fn find_agent(dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    if !SUPPORTED_ARCHES.contains(&arch) {
        return None;
    }
    dirs.iter()
        .flat_map(|d| {
            [
                d.join(format!("yonder-agent-{arch}-linux")),
                d.join(format!("{arch}-unknown-linux-musl/release/yonder-agent")),
            ]
        })
        .find(|p| p.is_file())
}

/// The host name that means this Mac: the agent runs here, without ssh.
pub const LOCAL_HOST: &str = "local";

/// The agent built for this machine, from the packaged
/// `yonder-agent-<arch>-macos` or a Cargo target directory.
fn find_local_agent(dirs: &[PathBuf]) -> Option<PathBuf> {
    let arch = std::env::consts::ARCH;
    dirs.iter()
        .flat_map(|d| {
            [
                d.join(format!("yonder-agent-{arch}-macos")),
                d.join(format!("{arch}-apple-darwin/release/yonder-agent")),
            ]
        })
        .find(|p| p.is_file())
}

/// One step of the connect sequence; `step` names it in errors.
struct Session {
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    log: LogFn,
    step: String,
}

impl Session {
    fn step(&mut self, step: impl Into<String>) {
        self.step = step.into();
        (self.log)(LogLine {
            level: Level::Step,
            message: self.step.clone(),
        });
    }

    /// Build an error for the current step, waiting briefly for ssh to exit
    /// so its stderr is complete.
    async fn fail(&mut self, message: impl Into<String>) -> ConnectError {
        let _ = timeout(Duration::from_secs(3), self.child.wait()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stderr: Vec<String> = self.stderr_tail.lock().unwrap().iter().cloned().collect();
        let mut message = message.into();
        if let Some(last) = stderr.iter().rev().find(|l| !l.trim().is_empty()) {
            message = format!("{message}: {last}");
        }
        ConnectError {
            step: self.step.clone(),
            hint: hint_for(&stderr.join("\n")),
            message,
        }
    }

    /// Read stdout lines until one starts with a marker from `wanted`. Other
    /// lines are login-shell noise and are only logged. The script prints each
    /// marker at the start of its own line, so noise that merely mentions one
    /// is not mistaken for it.
    async fn marker(&mut self, wanted: &[&str], limit: Duration) -> Result<String, ConnectError> {
        let read = async {
            let mut line = String::new();
            loop {
                line.clear();
                match self.stdout.read_line(&mut line).await {
                    Ok(0) => return Err("the remote side closed the connection".to_string()),
                    Ok(_) => {}
                    Err(e) => return Err(format!("reading from ssh failed: {e}")),
                }
                let text = line.trim();
                let token = text.split_whitespace().next().unwrap_or_default();
                if token == "YONDER-FAILED" {
                    return Err("installing the agent failed".into());
                }
                if wanted.contains(&token) {
                    return Ok(text.to_string());
                }
                if !text.is_empty() {
                    (self.log)(LogLine {
                        level: Level::Remote,
                        message: text.to_string(),
                    });
                }
            }
        };
        match timeout(limit, read).await {
            Ok(Ok(m)) => Ok(m),
            Ok(Err(e)) => Err(self.fail(e).await),
            Err(_) => Err(self
                .fail(format!("no answer after {} seconds", limit.as_secs()))
                .await),
        }
    }

    /// Skip whatever precedes the agent's magic bytes.
    async fn magic(&mut self, limit: Duration) -> Result<(), ConnectError> {
        let read = async {
            let mut window: Vec<u8> = Vec::new();
            loop {
                let b = self
                    .stdout
                    .read_u8()
                    .await
                    .map_err(|_| "the agent exited at startup")?;
                window.push(b);
                if window.ends_with(MAGIC) {
                    return Ok(());
                }
                if window.len() > 1 << 20 {
                    return Err("the agent did not identify itself");
                }
            }
        };
        match timeout(limit, read).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.fail(e).await),
            Err(_) => Err(self.fail("the agent did not start in time").await),
        }
    }
}

/// Start ssh, install the agent if needed, and return the running connection.
/// Every step is reported through `log`.
pub async fn connect(opts: &ConnectOptions, log: LogFn) -> Result<Connection, ConnectError> {
    let host = opts.host.trim();
    if host == LOCAL_HOST {
        return connect_local(opts, log).await;
    }
    if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
        return Err(ConnectError {
            step: "Checking the host".into(),
            message: format!("{host:?} is not a valid ssh destination"),
            hint: Some("Use a host name or a Host alias from ~/.ssh/config.".into()),
        });
    }

    log(LogLine {
        level: Level::Step,
        message: format!("Starting {} {host}", opts.ssh_program.display()),
    });
    let mut cmd = Command::new(&opts.ssh_program);
    match &opts.askpass {
        // ssh has no terminal here, so it asks through the app instead.
        Some(askpass) => {
            cmd.env("SSH_ASKPASS", &askpass.program)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(crate::askpass::SOCKET_ENV, &askpass.socket);
            // Older OpenSSH only uses SSH_ASKPASS when DISPLAY is set.
            if std::env::var_os("DISPLAY").is_none() {
                cmd.env("DISPLAY", ":0");
            }
        }
        // Fail with a clear message instead of waiting on a prompt that
        // nobody can see.
        None => {
            cmd.args(["-o", "BatchMode=yes"]);
        }
    }
    let child = cmd
        .args([
            "-T",
            "-o",
            "ConnectTimeout=15",
            // Notice a dead network within about a minute.
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
            host,
            &format!("sh -c '{REMOTE_SCRIPT}'"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ConnectError {
            step: "Starting ssh".into(),
            message: format!("could not run {}: {e}", opts.ssh_program.display()),
            hint: Some(
                "Yonder uses the OpenSSH client; make sure `ssh` works in a terminal.".into(),
            ),
        })?;

    let (mut s, mut stdin) = session(child, log);

    s.step(format!("Waiting for {host} to answer"));
    // Typing a password and a one-time code takes a while.
    let answer_time = if opts.askpass.is_some() { 300 } else { 45 };
    let hello = s
        .marker(&["YONDER-HELLO"], Duration::from_secs(answer_time))
        .await?;
    let mut fields = hello.split_whitespace().skip(1);
    let os = fields.next().unwrap_or_default().to_string();
    let arch = match fields.next().unwrap_or_default() {
        "arm64" => "aarch64".to_string(),
        a => a.to_string(),
    };
    if os != "Linux" {
        return Err(s
            .fail(format!("the remote runs {os:?}; only Linux is supported"))
            .await);
    }
    if !SUPPORTED_ARCHES.contains(&arch.as_str()) {
        return Err(s
            .fail(format!(
                "the remote CPU is {arch:?}; Yonder supports {}",
                SUPPORTED_ARCHES.join(" and ")
            ))
            .await);
    }

    s.step(format!(
        "Remote is Linux {arch}; looking for a matching agent"
    ));
    let Some(agent_path) = find_agent(&opts.agent_dirs, &arch) else {
        return Err(ConnectError {
            step: s.step.clone(),
            message: format!("this copy of Yonder has no agent built for {arch}"),
            hint: Some(
                "Build the agents with scripts/build-agents.sh, or set YONDER_AGENT_DIR.".into(),
            ),
        });
    };
    let agent = tokio::fs::read(&agent_path)
        .await
        .map_err(|e| ConnectError {
            step: s.step.clone(),
            message: format!("could not read {}: {e}", agent_path.display()),
            hint: None,
        })?;
    let hash = agent_hash(&agent);

    let offer = format!("{hash} {}\n", agent.len());
    if stdin.write_all(offer.as_bytes()).await.is_err() {
        return Err(s.fail("ssh closed its input").await);
    }
    let next = s
        .marker(&["YONDER-START", "YONDER-UPLOAD"], Duration::from_secs(30))
        .await?;
    if next == "YONDER-UPLOAD" {
        s.step(format!(
            "Installing agent {hash} ({} KB) in ~/.cache/yonder on the remote",
            agent.len() / 1024
        ));
        // If the remote fails part way (a full quota, say) the write breaks;
        // the marker below then reports what the remote said.
        let _ = stdin.write_all(&agent).await;
        let _ = stdin.flush().await;
        s.marker(&["YONDER-START"], Duration::from_secs(300))
            .await?;
    }

    finish(s, stdin).await
}

/// Run the agent on this machine, without ssh. It is copied to
/// `~/.cache/yonder` first, as on a remote, so the `yonder` command it links
/// beside itself is not written into the signed app bundle. It starts through
/// a login shell so it sees the `PATH` a terminal has (Homebrew, uv, …),
/// which an app opened from the Dock does not.
async fn connect_local(opts: &ConnectOptions, log: LogFn) -> Result<Connection, ConnectError> {
    let step = "Looking for the agent for this Mac";
    log(LogLine {
        level: Level::Step,
        message: step.into(),
    });
    let fail = |message: String, hint: Option<&str>| ConnectError {
        step: step.into(),
        message,
        hint: hint.map(Into::into),
    };
    let Some(agent_path) = find_local_agent(&opts.agent_dirs) else {
        return Err(fail(
            format!(
                "this copy of Yonder has no agent built for {}",
                std::env::consts::ARCH
            ),
            Some("Build the agents with scripts/build-agents.sh, or set YONDER_AGENT_DIR."),
        ));
    };
    let agent = tokio::fs::read(&agent_path).await.map_err(|e| {
        fail(
            format!("could not read {}: {e}", agent_path.display()),
            None,
        )
    })?;
    let home = std::env::var_os("HOME").ok_or_else(|| fail("HOME is not set".into(), None))?;
    let dir = PathBuf::from(home).join(".cache/yonder");
    let path = dir.join(format!("agent-{}", agent_hash(&agent)));
    if !path.is_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = dir.join(format!("agent.tmp.{}", std::process::id()));
        let installed = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&tmp, &agent))
            .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = installed {
            let _ = std::fs::remove_file(&tmp);
            return Err(fail(
                format!("could not install it in {}: {e}", dir.display()),
                None,
            ));
        }
    }
    // Single quotes read the same in sh, bash, zsh, csh and fish.
    let quoted = path.to_string_lossy().into_owned();
    if quoted.contains('\'') {
        return Err(fail(format!("{quoted} has a quote in it"), None));
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let child = Command::new(&shell)
        .args(["-l", "-c", &format!("exec '{quoted}'")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| fail(format!("could not run {shell}: {e}"), None))?;
    let (s, stdin) = session(child, log);
    finish(s, stdin).await
}

/// Watch a freshly started child: log its stderr and keep the last lines
/// for error messages.
fn session(mut child: Child, log: LogFn) -> (Session, ChildStdin) {
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    {
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let tail = Arc::clone(&stderr_tail);
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                log(LogLine {
                    level: Level::Remote,
                    message: line.clone(),
                });
                let mut tail = tail.lock().unwrap();
                if tail.len() == 50 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
    }
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let s = Session {
        child,
        stdout,
        stderr_tail,
        log,
        step: String::new(),
    };
    (s, stdin)
}

/// Past the agent's magic bytes: start routing and check that it answers.
async fn finish(mut s: Session, stdin: ChildStdin) -> Result<Connection, ConnectError> {
    s.step("Starting the agent");
    s.magic(Duration::from_secs(30)).await?;

    let Session {
        child,
        stdout,
        stderr_tail,
        log,
        ..
    } = s;
    // What ssh said while logging in (a mistyped password, say) is history;
    // a later disconnect should not be blamed on it.
    stderr_tail.lock().unwrap().clear();
    let conn = Connection::start(child, stdin, stdout, stderr_tail, Arc::clone(&log));
    let info = match timeout(Duration::from_secs(30), conn.call(Op::Hello)).await {
        Ok(Ok(Reply::Hello(info))) => info,
        other => {
            return Err(ConnectError {
                step: "Starting the agent".into(),
                message: format!("the agent did not answer correctly: {other:?}"),
                hint: None,
            })
        }
    };
    if info.protocol != PROTOCOL_VERSION {
        return Err(ConnectError {
            step: "Starting the agent".into(),
            message: format!(
                "agent speaks protocol {}, this app speaks {PROTOCOL_VERSION}",
                info.protocol
            ),
            hint: None,
        });
    }
    log(LogLine {
        level: Level::Step,
        message: format!(
            "Connected: agent {} on {} (pid {})",
            info.agent_version, info.hostname, info.pid
        ),
    });
    let _ = conn.info.set(info);
    Ok(conn)
}

fn agent_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Turn the usual ssh and remote failures into advice.
fn hint_for(stderr: &str) -> Option<String> {
    let s = stderr.to_ascii_lowercase();
    let hint = if s.contains("permission denied") {
        "Authentication failed. Check the password or one-time code, or that `ssh <host>` \
         works in a terminal; for keys, load them into ssh-agent or renew a short-lived key \
         or certificate."
    } else if s.contains("host key verification failed") {
        "The host key was not accepted, or it changed since you last connected. If it changed \
         unexpectedly, check with the system's administrators before trusting it."
    } else if s.contains("could not resolve hostname") {
        "Check the host name, or the Host entry in ~/.ssh/config."
    } else if s.contains("timed out") || s.contains("no route to host") {
        "The host did not respond. Check your network or VPN."
    } else if s.contains("connection refused") {
        "The host refused the connection. Check the host name and port."
    } else if s.contains("quota exceeded") || s.contains("no space left") {
        "The remote home directory is full. Yonder keeps its agent (about 1 MB) in \
         ~/.cache/yonder; free some space and connect again."
    } else if s.contains("exec format error") {
        "The agent does not match the remote CPU; please report this."
    } else {
        return None;
    };
    Some(hint.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_safe_for_any_login_shell() {
        assert!(!REMOTE_SCRIPT.contains('\''));
        assert!(!REMOTE_SCRIPT.contains('!'));
        assert!(!REMOTE_SCRIPT.contains('\n'));
    }

    #[test]
    fn hints() {
        assert!(hint_for("user@host: Permission denied (publickey).")
            .unwrap()
            .contains("Authentication failed"));
        assert!(hint_for("head: write error: Disk quota exceeded")
            .unwrap()
            .contains("cache"));
        assert!(hint_for("something else").is_none());
    }

    #[test]
    fn finds_packaged_and_cargo_layouts() {
        let d = std::env::temp_dir().join(format!("yonder-find-{}", std::process::id()));
        std::fs::create_dir_all(d.join("x86_64-unknown-linux-musl/release")).unwrap();
        std::fs::write(d.join("x86_64-unknown-linux-musl/release/yonder-agent"), "").unwrap();
        std::fs::write(d.join("yonder-agent-aarch64-linux"), "").unwrap();
        let dirs = vec![d.clone()];
        assert!(find_agent(&dirs, "x86_64").is_some());
        assert!(find_agent(&dirs, "aarch64").is_some());
        assert!(find_agent(&dirs, "riscv64").is_none());
        // Remote-supplied values never become paths outside `dirs`.
        assert!(find_agent(&dirs, "../x86_64").is_none());
        assert!(find_agent(&dirs, "/etc/passwd").is_none());
        std::fs::remove_dir_all(d).unwrap();
    }
}
