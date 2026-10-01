use crate::{Connection, Level, LogFn, LogLine};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
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
}

impl ConnectOptions {
    pub fn new(host: impl Into<String>, agent_dirs: Vec<PathBuf>) -> Self {
        ConnectOptions {
            host: host.into(),
            ssh_program: std::env::var_os("YONDER_SSH")
                .map(PathBuf::from)
                .unwrap_or_else(|| "ssh".into()),
            agent_dirs,
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

/// Look for the agent build for `arch` (as printed by `uname -m`) in `dirs`.
/// Accepts both the packaged name `yonder-agent-<arch>-linux` and a Cargo
/// target directory layout, `<arch>-unknown-linux-musl/release/yonder-agent`.
pub fn find_agent(dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    dirs.iter()
        .flat_map(|d| {
            [
                d.join(format!("yonder-agent-{arch}-linux")),
                d.join(format!("{arch}-unknown-linux-musl/release/yonder-agent")),
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

    /// Read stdout lines until one carries a marker from `wanted`. Other lines
    /// are login-shell noise and are only logged.
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
                if let Some(at) = text.find("YONDER-") {
                    let marker = &text[at..];
                    if marker == "YONDER-FAILED" {
                        return Err("installing the agent failed".into());
                    }
                    if wanted.iter().any(|w| marker.starts_with(w)) {
                        return Ok(marker.to_string());
                    }
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
    let mut child = Command::new(&opts.ssh_program)
        .args([
            "-T",
            // Fail with a clear message instead of waiting on a prompt that
            // nobody can see.
            "-o",
            "BatchMode=yes",
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
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut s = Session {
        child,
        stdout,
        stderr_tail: Arc::clone(&stderr_tail),
        log: Arc::clone(&log),
        step: String::new(),
    };

    s.step(format!("Waiting for {host} to answer"));
    let hello = s.marker(&["YONDER-HELLO"], Duration::from_secs(45)).await?;
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

    s.step("Starting the agent");
    s.magic(Duration::from_secs(30)).await?;

    let Session {
        child,
        stdout,
        log: session_log,
        ..
    } = s;
    let conn = Connection::start(child, stdin, stdout, stderr_tail, session_log);
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
        "Authentication failed. Yonder runs ssh without a terminal, so it cannot answer \
         password or MFA prompts. Check that `ssh -o BatchMode=yes <host> true` works in a \
         terminal: load your key into ssh-agent, or renew a short-lived key or certificate."
    } else if s.contains("host key verification failed") {
        "The host key is unknown or has changed. Connect once with plain `ssh` in a terminal \
         to review and accept it."
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
            .contains("BatchMode"));
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
        std::fs::remove_dir_all(d).unwrap();
    }
}
