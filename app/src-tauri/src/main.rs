//! Yonder's desktop process: owns the connection and exposes it to the UI as
//! Tauri commands. All editing state lives in the UI; this side only moves
//! bytes, so keystrokes never wait on it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, InvokeResponseBody, Response};
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, Wry};
use yonder_client::askpass::{self, Askpass};
use yonder_client::git::{self, Change, Commit, Found, Status};
use yonder_client::{connect as open_connection, ConnectOptions, Connection, Level, LogLine};
use yonder_proto::{EntryKind, Error as ProtoError, ErrorKind, Event, Op, Reply};

/// Text files larger than this are refused: the editor gets sluggish well
/// before, and such files are rarely meant to be edited by hand.
const MAX_OPEN_BYTES: u64 = 16 << 20;

/// Images and PDFs are shown, not edited, so they may be larger.
const MAX_VIEW_BYTES: u64 = 128 << 20;

#[derive(Default)]
struct AppState {
    /// Open connections by id, one per host the UI has workspaces on.
    conns: Mutex<HashMap<u64, Arc<Connection>>>,
    /// The last connection id handed out; ids are never reused.
    generation: AtomicU64,
    /// Terminals of each connection, by connection id.
    terminals: Arc<Mutex<HashMap<u64, Terminals>>>,
    /// Held for a whole connection attempt, so password prompts and their
    /// cancellation belong to one attempt at a time.
    connecting: tokio::sync::Mutex<()>,
    /// Lets ssh ask for passwords and one-time codes through the window.
    askpass: Mutex<Option<(askpass::Server, Askpass)>>,
    /// Questions from ssh waiting for the person's answer, by id.
    prompts: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Option<String>>>>>,
    /// Set when the person cancels a question: ssh would otherwise ask the
    /// same question again, so the rest of this login attempt is declined.
    prompts_cancelled: Arc<AtomicBool>,
    /// Where each stream's output goes, by stream id, with its connection.
    streams: Streams,
    /// Local ports that tunnel to a remote port, by connection and port.
    tunnels: Mutex<HashMap<(u64, u16), u16>>,
    /// Folders macOS asked us to open (`yonder .`), until the UI takes them.
    opened: Mutex<Vec<String>>,
}

type Streams = Arc<Mutex<HashMap<u64, (u64, Sink)>>>;

/// Stream ids are picked here, unique across connections.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

enum Sink {
    /// To the UI: each message is a tag byte and data. Tag 1 is stdout, 2
    /// stderr, 0 the end, followed by the exit code in decimal if any.
    Ui(Channel<InvokeResponseBody>),
    /// To a local socket; dropping the sender closes it.
    Tcp(tokio::sync::mpsc::UnboundedSender<Vec<u8>>),
}

impl Sink {
    fn end(self, code: Option<i32>) {
        if let Sink::Ui(ch) = self {
            let mut msg = vec![0];
            msg.extend(code.map(|c| c.to_string()).unwrap_or_default().bytes());
            let _ = ch.send(InvokeResponseBody::Raw(msg));
        }
    }
}

/// Where each terminal's output goes. Output can arrive before `pty_open`
/// has returned the terminal's id to the UI, so it is held until then.
#[derive(Default)]
struct Terminals {
    channels: HashMap<u64, Channel<InvokeResponseBody>>,
    early_output: HashMap<u64, Vec<Vec<u8>>>,
    early_exit: HashMap<u64, Option<i32>>,
}

#[derive(Serialize, Clone)]
struct PtyExitEvent {
    conn: u64,
    pty: u64,
    code: Option<i32>,
}

/// Route the agent's terminal and stream events until the connection ends.
fn forward_events(
    app: AppHandle,
    conn: &Connection,
    terminals: Arc<Mutex<HashMap<u64, Terminals>>>,
    streams: Streams,
    generation: u64,
) {
    let Some(mut events) = conn.take_events() else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                Event::StreamOutput { id, stderr, data } => {
                    match streams.lock().unwrap().get(&id) {
                        Some((_, Sink::Ui(ch))) => {
                            let mut msg = Vec::with_capacity(data.len() + 1);
                            msg.push(if stderr { 2 } else { 1 });
                            msg.extend_from_slice(&data);
                            let _ = ch.send(InvokeResponseBody::Raw(msg));
                        }
                        Some((_, Sink::Tcp(tx))) => {
                            let _ = tx.send(data);
                        }
                        None => {}
                    }
                    continue;
                }
                Event::StreamExit { id, code } => {
                    if let Some((_, sink)) = streams.lock().unwrap().remove(&id) {
                        sink.end(code);
                    }
                    continue;
                }
                _ => {}
            }
            let mut all = terminals.lock().unwrap();
            let Some(t) = all.get_mut(&generation) else {
                return;
            };
            match event {
                Event::PtyOutput { pty, data } => match t.channels.get(&pty) {
                    Some(ch) => {
                        let _ = ch.send(InvokeResponseBody::Raw(data));
                    }
                    None => t.early_output.entry(pty).or_default().push(data),
                },
                Event::StreamOutput { .. } | Event::StreamExit { .. } => {}
                Event::PtyExit { pty, code } => {
                    if t.channels.remove(&pty).is_some() {
                        let _ = app.emit(
                            "pty-exit",
                            PtyExitEvent {
                                conn: generation,
                                pty,
                                code,
                            },
                        );
                    } else {
                        t.early_exit.insert(pty, code);
                    }
                }
            }
        }
    });
}

impl AppState {
    fn get(&self, conn: u64) -> Result<Arc<Connection>, CmdError> {
        self.conns
            .lock()
            .unwrap()
            .get(&conn)
            .cloned()
            .ok_or_else(|| CmdError {
                kind: "disconnected",
                message: "not connected to the remote".into(),
                hint: None,
            })
    }
}

/// Errors as the UI sees them.
#[derive(Serialize, Debug)]
struct CmdError {
    kind: &'static str,
    message: String,
    hint: Option<String>,
}

impl From<ProtoError> for CmdError {
    fn from(e: ProtoError) -> Self {
        let kind = match e.kind {
            ErrorKind::NotFound => "not_found",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::IsDirectory => "is_directory",
            ErrorKind::NotDirectory => "not_directory",
            ErrorKind::TooLarge => "too_large",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Disconnected => "disconnected",
            ErrorKind::Other => "other",
        };
        CmdError {
            kind,
            message: e.message,
            hint: None,
        }
    }
}

fn unexpected(reply: Reply) -> CmdError {
    CmdError {
        kind: "other",
        message: format!("unexpected reply from the agent: {reply:?}"),
        hint: None,
    }
}

#[derive(Serialize, Clone)]
struct LogEvent {
    level: &'static str,
    message: String,
}

#[derive(Serialize, Clone)]
struct ClosedEvent {
    generation: u64,
    reason: String,
}

#[derive(Serialize)]
struct ConnInfo {
    generation: u64,
    host: String,
    root: String,
    hostname: String,
    home: String,
}

/// Where agent builds may be found, most specific first.
fn agent_dirs(app: &AppHandle) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("YONDER_AGENT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(res) = app.path().resource_dir() {
        dirs.push(res.join("agents"));
    }
    if cfg!(debug_assertions) {
        // `npm run tauri dev`: use the agents built in this checkout.
        dirs.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/agents"
        )));
        dirs.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target"
        )));
    }
    dirs
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
    path: String,
) -> Result<ConnInfo, CmdError> {
    let _attempt = state.connecting.lock().await;
    let generation = state.generation.fetch_add(1, Ordering::Relaxed) + 1;

    let log_app = app.clone();
    let log = Arc::new(move |line: LogLine| {
        let level = match line.level {
            Level::Step => "step",
            Level::Warn => "warn",
            Level::Remote => "remote",
        };
        let _ = log_app.emit(
            "conn-log",
            LogEvent {
                level,
                message: line.message,
            },
        );
    });
    state.prompts_cancelled.store(false, Ordering::Relaxed);
    let mut opts = ConnectOptions::new(host.clone(), agent_dirs(&app));
    opts.askpass = state
        .askpass
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, a)| a.clone());
    let conn = open_connection(&opts, log).await.map_err(|e| CmdError {
        kind: "connect",
        message: format!("{}: {}", e.step, e.message),
        hint: e.hint,
    })?;

    let root = resolve_folder(&conn, path).await?;

    let conn = Arc::new(conn);
    let info = ConnInfo {
        generation,
        host,
        root,
        hostname: conn.info().hostname.clone(),
        home: conn.info().home.clone(),
    };
    state
        .conns
        .lock()
        .unwrap()
        .insert(generation, Arc::clone(&conn));
    state
        .terminals
        .lock()
        .unwrap()
        .insert(generation, Terminals::default());
    forward_events(
        app.clone(),
        &conn,
        Arc::clone(&state.terminals),
        Arc::clone(&state.streams),
        generation,
    );

    let watcher = Arc::clone(&conn);
    tauri::async_runtime::spawn(async move {
        let reason = watcher.closed().await;
        let state = app.state::<AppState>();
        state.conns.lock().unwrap().remove(&generation);
        state.terminals.lock().unwrap().remove(&generation);
        let ended: Vec<Sink> = {
            let mut streams = state.streams.lock().unwrap();
            let ids: Vec<u64> = streams
                .iter()
                .filter(|(_, (c, _))| *c == generation)
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| streams.remove(id))
                .map(|(_, s)| s)
                .collect()
        };
        for sink in ended {
            sink.end(None);
        }
        state
            .tunnels
            .lock()
            .unwrap()
            .retain(|(c, _), _| *c != generation);
        let _ = app.emit("conn-closed", ClosedEvent { generation, reason });
    });
    Ok(info)
}

async fn resolve_folder(conn: &Connection, path: String) -> Result<String, CmdError> {
    match conn.call(Op::Resolve { path }).await? {
        Reply::Path { path, is_dir: true } => Ok(path),
        Reply::Path { path, .. } => Err(CmdError {
            kind: "not_directory",
            message: format!("{path} is not a folder"),
            hint: None,
        }),
        other => Err(unexpected(other)),
    }
}

/// What the connect screen offers: this Mac, then the `Host` aliases in
/// `~/.ssh/config`.
#[tauri::command]
fn ssh_hosts() -> Vec<String> {
    let mut hosts = vec![yonder_client::LOCAL_HOST.to_string()];
    if let Some(home) = std::env::var_os("HOME") {
        let config = PathBuf::from(home).join(".ssh/config");
        hosts.extend(
            yonder_client::ssh_config::hosts(&config)
                .into_iter()
                .filter(|h| h != yonder_client::LOCAL_HOST),
        );
    }
    hosts
}

/// Another folder on the connected host: no new ssh session.
#[tauri::command]
async fn open_folder(
    state: State<'_, AppState>,
    conn: u64,
    path: String,
) -> Result<String, CmdError> {
    let conn = state.get(conn)?;
    resolve_folder(&conn, path).await
}

#[tauri::command]
fn disconnect(state: State<'_, AppState>, conn: u64) {
    if let Some(conn) = state.conns.lock().unwrap().remove(&conn) {
        conn.close();
    }
}

#[derive(Serialize)]
struct EntryOut {
    name: String,
    kind: &'static str,
    symlink: bool,
    size: u64,
}

#[tauri::command]
async fn list_dir(
    state: State<'_, AppState>,
    conn: u64,
    path: String,
) -> Result<Vec<EntryOut>, CmdError> {
    match state.get(conn)?.call(Op::ListDir { path }).await? {
        Reply::Entries(entries) => Ok(entries
            .into_iter()
            .map(|e| EntryOut {
                kind: match e.kind {
                    EntryKind::Dir => "dir",
                    EntryKind::File => "file",
                    EntryKind::BrokenLink => "broken",
                    EntryKind::Other => "other",
                },
                name: e.name,
                symlink: e.symlink,
                size: e.stat.size,
            })
            .collect()),
        other => Err(unexpected(other)),
    }
}

#[derive(Serialize)]
struct FileOut {
    /// `None` for files that are not UTF-8 text.
    text: Option<String>,
    /// Content hash as hex: JavaScript numbers cannot hold a u64 exactly.
    hash: String,
    size: u64,
    /// As [`StatOut::version`], so a later stat shows whether it changed.
    version: String,
}

fn version_of(s: &yonder_proto::FileStat) -> String {
    format!("{}:{}.{:09}", s.size, s.mtime_s, s.mtime_ns)
}

#[tauri::command]
async fn read_file(
    state: State<'_, AppState>,
    conn: u64,
    path: String,
) -> Result<FileOut, CmdError> {
    let op = Op::ReadFile {
        path,
        max_bytes: MAX_OPEN_BYTES,
    };
    match state.get(conn)?.call(op).await? {
        Reply::File { data, hash, stat } => {
            let sniff = &data[..data.len().min(8192)];
            let text = if sniff.contains(&0) {
                None
            } else {
                String::from_utf8(data).ok()
            };
            Ok(FileOut {
                text,
                hash: format!("{hash:016x}"),
                size: stat.size,
                version: version_of(&stat),
            })
        }
        other => Err(unexpected(other)),
    }
}

/// The raw bytes of a file, for the image and PDF viewers. Sent as binary,
/// not JSON, so a 20 MB PDF does not become an 80 MB array of numbers.
#[tauri::command]
async fn read_bytes(
    state: State<'_, AppState>,
    conn: u64,
    path: String,
) -> Result<Response, CmdError> {
    let op = Op::ReadFile {
        path,
        max_bytes: MAX_VIEW_BYTES,
    };
    match state.get(conn)?.call(op).await? {
        Reply::File { data, .. } => Ok(Response::new(data)),
        other => Err(unexpected(other)),
    }
}

#[derive(Serialize)]
struct StatOut {
    size: u64,
    /// Changes whenever the size or modification time does.
    version: String,
}

#[tauri::command]
async fn stat(state: State<'_, AppState>, conn: u64, path: String) -> Result<StatOut, CmdError> {
    match state.get(conn)?.call(Op::Stat { path }).await? {
        Reply::Stat(s) => Ok(StatOut {
            size: s.size,
            version: version_of(&s),
        }),
        other => Err(unexpected(other)),
    }
}

#[derive(Serialize)]
struct PtyOpened {
    pty: u64,
    /// Set if the shell already exited, e.g. a login file that runs `exit`.
    /// Its `pty-exit` event went out before the UI knew this id.
    exited: bool,
    code: Option<i32>,
}

/// Start a shell on a new terminal; its output streams to `output` as raw
/// bytes.
#[tauri::command]
async fn pty_open(
    state: State<'_, AppState>,
    conn: u64,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    output: Channel<InvokeResponseBody>,
) -> Result<PtyOpened, CmdError> {
    let pty = match state
        .get(conn)?
        .call(Op::PtyOpen { cols, rows, cwd })
        .await?
    {
        Reply::Pty { pty } => pty,
        other => return Err(unexpected(other)),
    };
    let mut all = state.terminals.lock().unwrap();
    let Some(t) = all.get_mut(&conn) else {
        return Err(CmdError {
            kind: "disconnected",
            message: "not connected to the remote".into(),
            hint: None,
        });
    };
    for data in t.early_output.remove(&pty).unwrap_or_default() {
        let _ = output.send(InvokeResponseBody::Raw(data));
    }
    let early_exit = t.early_exit.remove(&pty);
    if early_exit.is_none() {
        t.channels.insert(pty, output);
    }
    Ok(PtyOpened {
        pty,
        exited: early_exit.is_some(),
        code: early_exit.flatten(),
    })
}

/// Keystrokes. Not async, so calls run one after another in the order the UI
/// made them, and nothing waits for the remote to answer.
#[tauri::command]
fn pty_write(
    state: State<'_, AppState>,
    conn: u64,
    pty: u64,
    data: String,
) -> Result<(), CmdError> {
    let data = data.into_bytes();
    Ok(state.get(conn)?.send(Op::PtyInput { pty, data })?)
}

/// The UI has drawn `bytes` more output; lets the agent send more.
#[tauri::command]
fn pty_ack(state: State<'_, AppState>, conn: u64, pty: u64, bytes: u64) -> Result<(), CmdError> {
    Ok(state.get(conn)?.send(Op::PtyAck { pty, bytes })?)
}

#[tauri::command]
async fn pty_resize(
    state: State<'_, AppState>,
    conn: u64,
    pty: u64,
    cols: u16,
    rows: u16,
) -> Result<(), CmdError> {
    state
        .get(conn)?
        .call(Op::PtyResize { pty, cols, rows })
        .await?;
    Ok(())
}

#[tauri::command]
async fn pty_close(state: State<'_, AppState>, conn: u64, pty: u64) -> Result<(), CmdError> {
    {
        if let Some(t) = state.terminals.lock().unwrap().get_mut(&conn) {
            t.channels.remove(&pty);
            t.early_output.remove(&pty);
            t.early_exit.remove(&pty);
        }
    }
    state.get(conn)?.call(Op::PtyClose { pty }).await?;
    Ok(())
}

// ---- streams: language servers, Jupyter, and tunnels to remote ports

/// Run `script` with sh in `cwd` on the remote; its output goes to `output`
/// (see [`Sink::Ui`]). Returns the stream's id.
#[tauri::command]
async fn proc_open(
    state: State<'_, AppState>,
    conn: u64,
    cwd: String,
    script: String,
    output: Channel<InvokeResponseBody>,
) -> Result<u64, CmdError> {
    let c = state.get(conn)?;
    let id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    // Registered first: output can arrive before the reply.
    state
        .streams
        .lock()
        .unwrap()
        .insert(id, (conn, Sink::Ui(output)));
    if let Err(e) = c.call(Op::ProcOpen { id, cwd, script }).await {
        state.streams.lock().unwrap().remove(&id);
        return Err(e.into());
    }
    Ok(id)
}

/// Bytes for a process's stdin. Not async, so writes keep their order.
#[tauri::command]
fn stream_write(
    state: State<'_, AppState>,
    conn: u64,
    id: u64,
    data: String,
) -> Result<(), CmdError> {
    let data = data.into_bytes();
    Ok(state.get(conn)?.send(Op::StreamInput { id, data })?)
}

#[tauri::command]
async fn stream_close(state: State<'_, AppState>, conn: u64, id: u64) -> Result<(), CmdError> {
    state.streams.lock().unwrap().remove(&id);
    state.get(conn)?.call(Op::StreamClose { id }).await?;
    Ok(())
}

/// A port on this Mac's loopback that leads to `port` on the remote's,
/// through the agent: no second ssh login. Kept until the connection ends.
#[tauri::command]
async fn tunnel_open(state: State<'_, AppState>, conn: u64, port: u16) -> Result<u16, CmdError> {
    let c = state.get(conn)?;
    if let Some(local) = state.tunnels.lock().unwrap().get(&(conn, port)) {
        return Ok(*local);
    }
    let io_err = |e: std::io::Error| CmdError {
        kind: "other",
        message: format!("could not listen on this Mac: {e}"),
        hint: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(io_err)?;
    let local = listener.local_addr().map_err(io_err)?.port();
    state.tunnels.lock().unwrap().insert((conn, port), local);
    let streams = Arc::clone(&state.streams);
    tauri::async_runtime::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            if c.is_closed() {
                return;
            }
            let _ = sock.set_nodelay(true);
            tauri::async_runtime::spawn(tunnel(
                Arc::clone(&c),
                Arc::clone(&streams),
                conn,
                port,
                sock,
            ));
        }
    });
    Ok(local)
}

/// Carry one local connection to the remote port and back.
async fn tunnel(
    c: Arc<Connection>,
    streams: Streams,
    conn: u64,
    port: u16,
    sock: tokio::net::TcpStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    streams.lock().unwrap().insert(id, (conn, Sink::Tcp(tx)));
    if c.call(Op::TcpOpen { id, port }).await.is_err() {
        streams.lock().unwrap().remove(&id);
        return;
    }
    let (mut read, mut write) = sock.into_split();
    let down = tauri::async_runtime::spawn(async move {
        while let Some(data) = rx.recv().await {
            if write.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = buf[..n].to_vec();
                if c.send(Op::StreamInput { id, data }).is_err() {
                    break;
                }
            }
        }
    }
    let _ = c.send(Op::StreamClose { id });
    streams.lock().unwrap().remove(&id);
    let _ = down.await;
}

// ---- git (read-only)

#[derive(Serialize)]
struct GitStatusOut {
    /// The repository's top folder; `None` if `dir` is not in a repository.
    repo: Option<String>,
    status: Option<Status>,
}

#[tauri::command]
async fn git_status(
    state: State<'_, AppState>,
    conn: u64,
    dir: String,
) -> Result<GitStatusOut, CmdError> {
    let conn = state.get(conn)?;
    let Some(repo) = git::repo_root(&conn, &dir).await? else {
        return Ok(GitStatusOut {
            repo: None,
            status: None,
        });
    };
    let status = git::status(&conn, &repo).await?;
    Ok(GitStatusOut {
        repo: Some(repo),
        status: Some(status),
    })
}

#[tauri::command]
async fn git_log(
    state: State<'_, AppState>,
    conn: u64,
    repo: String,
    skip: u32,
    limit: u32,
) -> Result<Vec<Commit>, CmdError> {
    let conn = state.get(conn)?;
    Ok(git::log(&conn, &repo, skip, limit).await?)
}

#[tauri::command]
async fn git_commit_files(
    state: State<'_, AppState>,
    conn: u64,
    repo: String,
    hash: String,
) -> Result<Vec<Change>, CmdError> {
    let conn = state.get(conn)?;
    Ok(git::commit_files(&conn, &repo, &hash).await?)
}

/// Every file under `dir` that git does not ignore, relative to `dir`, for
/// Go to File. `None` when `dir` is not in a repository.
#[tauri::command]
async fn git_files(
    state: State<'_, AppState>,
    conn: u64,
    dir: String,
) -> Result<Option<Vec<String>>, CmdError> {
    let conn = state.get(conn)?;
    Ok(git::files(&conn, &dir).await?)
}

/// Lines matching `query` in the files under `dir` that git does not ignore.
#[tauri::command]
async fn search(
    state: State<'_, AppState>,
    conn: u64,
    dir: String,
    query: git::Query,
) -> Result<Found, CmdError> {
    let conn = state.get(conn)?;
    Ok(git::search(&conn, &dir, &query).await?)
}

#[derive(Serialize)]
struct DiffOut {
    /// Both sides as text; `None` when either side is not UTF-8 text.
    original: Option<String>,
    modified: Option<String>,
}

/// Both sides of one file's change. Without `rev`: the last commit against
/// the working tree. With `rev`: that commit's parent against the commit.
#[tauri::command]
async fn git_diff(
    state: State<'_, AppState>,
    conn: u64,
    repo: String,
    path: String,
    old_path: Option<String>,
    rev: Option<String>,
) -> Result<DiffOut, CmdError> {
    let conn = state.get(conn)?;
    let before = old_path.as_deref().unwrap_or(&path);
    let (original, modified) = match &rev {
        Some(rev) => {
            let parent = format!("{rev}^");
            (
                git::file_at(&conn, &repo, &parent, before).await?,
                git::file_at(&conn, &repo, rev, &path).await?,
            )
        }
        None => {
            let original = git::file_at(&conn, &repo, "HEAD", before).await?;
            let full = format!("{}/{path}", repo.trim_end_matches('/'));
            // Git stores a symbolic link as its target path, so compare the
            // link itself, not the file it points to.
            let modified = match conn.call(Op::ReadLink { path: full.clone() }).await {
                Ok(Reply::Path { path: target, .. }) => Some(target.into_bytes()),
                Err(e) if e.kind == ErrorKind::NotFound => None,
                // Not a link: compare its contents.
                Err(_) => {
                    let op = Op::ReadFile {
                        path: full,
                        max_bytes: git::MAX_DIFF_FILE,
                    };
                    match conn.call(op).await {
                        Ok(Reply::File { data, .. }) => Some(data),
                        Err(e) if e.kind == ErrorKind::NotFound => None,
                        Err(e) => return Err(e.into()),
                        Ok(other) => return Err(unexpected(other)),
                    }
                }
                Ok(other) => return Err(unexpected(other)),
            };
            (original, modified)
        }
    };
    let text = |side: Option<Vec<u8>>| -> Option<String> {
        let data = side.unwrap_or_default();
        if data[..data.len().min(8192)].contains(&0) {
            return None;
        }
        String::from_utf8(data).ok()
    };
    let (original, modified) = (text(original), text(modified));
    if original.is_none() || modified.is_none() {
        return Ok(DiffOut {
            original: None,
            modified: None,
        });
    }
    Ok(DiffOut { original, modified })
}

#[derive(Serialize)]
struct WrittenOut {
    hash: String,
    size: u64,
    version: String,
}

/// Save `text`. With `expected_hash` the save is refused (kind "conflict") if
/// the file changed on the remote since it was read.
#[tauri::command]
async fn write_file(
    state: State<'_, AppState>,
    conn: u64,
    path: String,
    text: String,
    expected_hash: Option<String>,
) -> Result<WrittenOut, CmdError> {
    let expected_hash = match expected_hash {
        Some(h) => Some(u64::from_str_radix(&h, 16).map_err(|_| CmdError {
            kind: "other",
            message: format!("bad hash {h:?}"),
            hint: None,
        })?),
        None => None,
    };
    let op = Op::WriteFile {
        path,
        data: text.into_bytes(),
        expected_hash,
    };
    match state.get(conn)?.call(op).await? {
        Reply::Written { hash, stat } => Ok(WrittenOut {
            hash: format!("{hash:016x}"),
            size: stat.size,
            version: version_of(&stat),
        }),
        other => Err(unexpected(other)),
    }
}

// ---- ssh questions (passwords, one-time codes, host keys)

#[derive(Serialize, Clone)]
struct AskpassEvent {
    id: u64,
    prompt: String,
}

/// Start answering ssh's questions through the UI. Each question becomes an
/// `askpass` event; the UI replies with `askpass_answer`.
fn start_askpass(app: &AppHandle) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let state = app.state::<AppState>();
    let prompts = Arc::clone(&state.prompts);
    let cancelled = Arc::clone(&state.prompts_cancelled);
    let handle = app.clone();
    let handler: askpass::Handler = Arc::new(move |prompt: String| {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        prompts.lock().unwrap().insert(id, tx);
        let _ = handle.emit("askpass", AskpassEvent { id, prompt });
        // ssh gives up on its own after a while; so do we.
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(300))
            .ok()
            .flatten();
        prompts.lock().unwrap().remove(&id);
        answer
    });
    match askpass::Server::start(handler).and_then(|s| s.askpass().map(|a| (s, a))) {
        Ok(pair) => *state.askpass.lock().unwrap() = Some(pair),
        // Without it, ssh runs in batch mode: keys only, as before.
        Err(e) => eprintln!("yonder: password prompts unavailable: {e}"),
    }
}

/// The person's answer to an ssh question; `None` cancels.
#[tauri::command]
fn askpass_answer(state: State<'_, AppState>, id: u64, answer: Option<String>) {
    if answer.is_none() {
        state.prompts_cancelled.store(true, Ordering::Relaxed);
    }
    if let Some(tx) = state.prompts.lock().unwrap().remove(&id) {
        let _ = tx.send(answer);
    }
}

// ---- quitting with unsaved edits

/// Quit now: the UI found no unsaved edits, or the person chose to discard
/// them.
#[tauri::command]
fn quit_app(app: AppHandle, state: State<'_, AppState>) {
    for (_, conn) in state.conns.lock().unwrap().drain() {
        conn.close();
    }
    app.exit(0);
}

/// Puts text on the clipboard. The web clipboard API wants a click in the
/// page, which a menu item is not.
#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("pbcopy: {e}"))?;
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    child.wait().map_err(|e| format!("pbcopy: {e}"))?;
    written.map_err(|e| format!("pbcopy: {e}"))
}

/// Puts a PNG on the clipboard, and nothing else. WebKit's own Copy Image
/// adds HTML pointing at a blob: URL only this app can open, which web apps
/// such as Google Slides try to fetch and fail.
#[tauri::command]
fn copy_png(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("expected PNG bytes".into());
    };
    let path = std::env::temp_dir().join(format!("yonder-clip-{}.png", std::process::id()));
    std::fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    let script = format!(
        "set the clipboard to (read (POSIX file \"{}\") as «class PNGf»)",
        path.display()
    );
    let out = std::process::Command::new("osascript")
        .args(["-e", &script])
        .output();
    let _ = std::fs::remove_file(&path);
    let out = out.map_err(|e| format!("osascript: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Folders handed to the app by macOS since the UI last asked.
#[tauri::command]
fn take_opened(state: State<'_, AppState>) -> Vec<String> {
    std::mem::take(&mut state.opened.lock().unwrap())
}

/// Whether a quit may go ahead now. While a window is open, the UI decides:
/// it checks for unsaved edits at that moment, asks if there are any, and
/// calls `quit_app`. A copy of that state kept here could be stale.
fn request_quit(app: &AppHandle) -> bool {
    if app.webview_windows().is_empty() {
        // The window already closed, after the UI's own check.
        return true;
    }
    let _ = app.emit("quit-requested", ());
    false
}

/// The standard macOS menus, except that Quit goes through `request_quit`
/// and the File items are handled by the UI (menu shortcuts work even while
/// the editor or a terminal has the keyboard).
fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let quit = MenuItemBuilder::with_id("quit", "Quit Yonder")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Yonder")
        .about(None)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(&quit)
        .build()?;
    let file = SubmenuBuilder::new(app, "File")
        .item(
            &MenuItemBuilder::with_id("switch-workspace", "Switch Workspace…")
                .accelerator("CmdOrCtrl+Alt+O")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("new-workspace", "New Workspace…")
                .accelerator("CmdOrCtrl+Shift+N")
                .build(app)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id("go-to-file", "Go to File…")
                .accelerator("CmdOrCtrl+P")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("find-in-folder", "Find in Folder…")
                .accelerator("CmdOrCtrl+Shift+F")
                .build(app)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id("copy-path", "Copy Path")
                .accelerator("CmdOrCtrl+Alt+C")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("copy-relative-path", "Copy Relative Path")
                .accelerator("CmdOrCtrl+Alt+Shift+C")
                .build(app)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id("close-tab", "Close Tab")
                .accelerator("CmdOrCtrl+W")
                .build(app)?,
        )
        .build()?;
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let view = SubmenuBuilder::new(app, "View")
        .item(
            &MenuItemBuilder::with_id("toggle-sidebar", "Toggle Sidebar")
                .accelerator("CmdOrCtrl+B")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("markdown-preview", "Toggle Markdown Preview")
                .accelerator("CmdOrCtrl+Shift+V")
                .build(app)?,
        )
        .build()?;
    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize()
        .build()?;
    MenuBuilder::new(app)
        .items(&[&app_menu, &file, &edit, &view, &window])
        .build()
}

fn main() {
    // Started by ssh as SSH_ASKPASS: pass the question to the running app,
    // print its answer, and exit without opening a window.
    if let Some(socket) = std::env::var_os(askpass::SOCKET_ENV) {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        std::process::exit(askpass::client(std::path::Path::new(&socket), &prompt));
    }

    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            connect,
            ssh_hosts,
            open_folder,
            disconnect,
            list_dir,
            read_file,
            read_bytes,
            stat,
            write_file,
            pty_open,
            pty_write,
            pty_ack,
            pty_resize,
            pty_close,
            git_status,
            git_log,
            git_commit_files,
            git_diff,
            git_files,
            search,
            proc_open,
            stream_write,
            stream_close,
            tunnel_open,
            quit_app,
            copy_text,
            copy_png,
            askpass_answer,
            take_opened
        ])
        .setup(|app| {
            start_askpass(app.handle());
            Ok(())
        })
        .menu(build_menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "quit" if request_quit(app) => app.exit(0),
            id @ ("go-to-file" | "copy-path" | "copy-relative-path" | "close-tab"
            | "toggle-sidebar" | "markdown-preview" | "switch-workspace"
            | "new-workspace" | "find-in-folder") => {
                let _ = app.emit("menu", id);
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("error while starting Yonder")
        .run(|app, event| {
            // `open -a Yonder <folder>`, as the `yonder` script does. This can
            // come before the UI listens, so it is kept until the UI takes it.
            #[cfg(target_os = "macos")]
            if let RunEvent::Opened { urls } = &event {
                let paths = urls.iter().filter_map(|u| u.to_file_path().ok());
                app.state::<AppState>()
                    .opened
                    .lock()
                    .unwrap()
                    .extend(paths.map(|p| p.to_string_lossy().into_owned()));
                let _ = app.emit("opened", ());
            }
            // Quitting from the Dock, or the last window closing: the UI
            // decides. `code` is set when we exit on purpose.
            if let RunEvent::ExitRequested {
                api, code: None, ..
            } = event
            {
                if !request_quit(app) {
                    api.prevent_exit();
                }
            }
        });
}
