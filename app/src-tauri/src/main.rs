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
use yonder_client::git::{self, Change, Commit, Status};
use yonder_client::{connect as open_connection, ConnectOptions, Connection, Level, LogLine};
use yonder_proto::{EntryKind, Error as ProtoError, ErrorKind, Event, Op, Reply};

/// Text files larger than this are refused: the editor gets sluggish well
/// before, and such files are rarely meant to be edited by hand.
const MAX_OPEN_BYTES: u64 = 16 << 20;

/// Images and PDFs are shown, not edited, so they may be larger.
const MAX_VIEW_BYTES: u64 = 128 << 20;

#[derive(Default)]
struct AppState {
    conn: Mutex<Option<Arc<Connection>>>,
    /// Identifies a connection, so the UI can ignore a late "closed" event
    /// from one it already replaced.
    generation: AtomicU64,
    terminals: Arc<Mutex<Terminals>>,
    /// The UI has unsaved edits; quitting must ask first.
    unsaved: AtomicBool,
    /// Lets ssh ask for passwords and one-time codes through the window.
    askpass: Mutex<Option<(askpass::Server, Askpass)>>,
    /// Questions from ssh waiting for the person's answer, by id.
    prompts: Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Option<String>>>>>,
    /// Set when the person cancels a question: ssh would otherwise ask the
    /// same question again, so the rest of this login attempt is declined.
    prompts_cancelled: Arc<AtomicBool>,
}

/// Where each terminal's output goes. Output can arrive before `pty_open`
/// has returned the terminal's id to the UI, so it is held until then.
#[derive(Default)]
struct Terminals {
    /// The connection these terminals belong to; ids restart with each agent.
    generation: u64,
    channels: HashMap<u64, Channel<InvokeResponseBody>>,
    early_output: HashMap<u64, Vec<Vec<u8>>>,
    early_exit: HashMap<u64, Option<i32>>,
}

#[derive(Serialize, Clone)]
struct PtyExitEvent {
    pty: u64,
    code: Option<i32>,
}

/// Route the agent's terminal events to the UI until the connection ends.
fn forward_events(
    app: AppHandle,
    conn: &Connection,
    terminals: Arc<Mutex<Terminals>>,
    generation: u64,
) {
    let Some(mut events) = conn.take_events() else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            let mut t = terminals.lock().unwrap();
            if t.generation != generation {
                return;
            }
            match event {
                Event::PtyOutput { pty, data } => match t.channels.get(&pty) {
                    Some(ch) => {
                        let _ = ch.send(InvokeResponseBody::Raw(data));
                    }
                    None => t.early_output.entry(pty).or_default().push(data),
                },
                Event::PtyExit { pty, code } => {
                    if t.channels.remove(&pty).is_some() {
                        let _ = app.emit("pty-exit", PtyExitEvent { pty, code });
                    } else {
                        t.early_exit.insert(pty, code);
                    }
                }
            }
        }
    });
}

impl AppState {
    fn current(&self) -> Result<Arc<Connection>, CmdError> {
        self.conn.lock().unwrap().clone().ok_or_else(|| CmdError {
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
    if let Some(old) = state.conn.lock().unwrap().take() {
        old.close();
    }
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

    let root = match conn.call(Op::Resolve { path: path.clone() }).await? {
        Reply::Path { path, is_dir: true } => path,
        Reply::Path { path, .. } => {
            return Err(CmdError {
                kind: "not_directory",
                message: format!("{path} is not a folder"),
                hint: None,
            })
        }
        other => return Err(unexpected(other)),
    };

    let conn = Arc::new(conn);
    let info = ConnInfo {
        generation,
        host,
        root,
        hostname: conn.info().hostname.clone(),
        home: conn.info().home.clone(),
    };
    *state.conn.lock().unwrap() = Some(Arc::clone(&conn));
    *state.terminals.lock().unwrap() = Terminals {
        generation,
        ..Default::default()
    };
    forward_events(app.clone(), &conn, Arc::clone(&state.terminals), generation);

    let watcher = Arc::clone(&conn);
    tauri::async_runtime::spawn(async move {
        let reason = watcher.closed().await;
        let _ = app.emit("conn-closed", ClosedEvent { generation, reason });
    });
    Ok(info)
}

#[tauri::command]
fn disconnect(state: State<'_, AppState>) {
    if let Some(conn) = state.conn.lock().unwrap().take() {
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
async fn list_dir(state: State<'_, AppState>, path: String) -> Result<Vec<EntryOut>, CmdError> {
    match state.current()?.call(Op::ListDir { path }).await? {
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
}

#[tauri::command]
async fn read_file(state: State<'_, AppState>, path: String) -> Result<FileOut, CmdError> {
    let op = Op::ReadFile {
        path,
        max_bytes: MAX_OPEN_BYTES,
    };
    match state.current()?.call(op).await? {
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
            })
        }
        other => Err(unexpected(other)),
    }
}

/// The raw bytes of a file, for the image and PDF viewers. Sent as binary,
/// not JSON, so a 20 MB PDF does not become an 80 MB array of numbers.
#[tauri::command]
async fn read_bytes(state: State<'_, AppState>, path: String) -> Result<Response, CmdError> {
    let op = Op::ReadFile {
        path,
        max_bytes: MAX_VIEW_BYTES,
    };
    match state.current()?.call(op).await? {
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
async fn stat(state: State<'_, AppState>, path: String) -> Result<StatOut, CmdError> {
    match state.current()?.call(Op::Stat { path }).await? {
        Reply::Stat(s) => Ok(StatOut {
            size: s.size,
            version: format!("{}:{}.{:09}", s.size, s.mtime_s, s.mtime_ns),
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
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    output: Channel<InvokeResponseBody>,
) -> Result<PtyOpened, CmdError> {
    let pty = match state
        .current()?
        .call(Op::PtyOpen { cols, rows, cwd })
        .await?
    {
        Reply::Pty { pty } => pty,
        other => return Err(unexpected(other)),
    };
    let mut t = state.terminals.lock().unwrap();
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
fn pty_write(state: State<'_, AppState>, pty: u64, data: String) -> Result<(), CmdError> {
    let data = data.into_bytes();
    Ok(state.current()?.send(Op::PtyInput { pty, data })?)
}

/// The UI has drawn `bytes` more output; lets the agent send more.
#[tauri::command]
fn pty_ack(state: State<'_, AppState>, pty: u64, bytes: u64) -> Result<(), CmdError> {
    Ok(state.current()?.send(Op::PtyAck { pty, bytes })?)
}

#[tauri::command]
async fn pty_resize(
    state: State<'_, AppState>,
    pty: u64,
    cols: u16,
    rows: u16,
) -> Result<(), CmdError> {
    state
        .current()?
        .call(Op::PtyResize { pty, cols, rows })
        .await?;
    Ok(())
}

#[tauri::command]
async fn pty_close(state: State<'_, AppState>, pty: u64) -> Result<(), CmdError> {
    {
        let mut t = state.terminals.lock().unwrap();
        t.channels.remove(&pty);
        t.early_output.remove(&pty);
        t.early_exit.remove(&pty);
    }
    state.current()?.call(Op::PtyClose { pty }).await?;
    Ok(())
}

// ---- git (read-only)

#[derive(Serialize)]
struct GitStatusOut {
    /// The repository's top folder; `None` if `dir` is not in a repository.
    repo: Option<String>,
    status: Option<Status>,
}

#[tauri::command]
async fn git_status(state: State<'_, AppState>, dir: String) -> Result<GitStatusOut, CmdError> {
    let conn = state.current()?;
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
    repo: String,
    skip: u32,
    limit: u32,
) -> Result<Vec<Commit>, CmdError> {
    let conn = state.current()?;
    Ok(git::log(&conn, &repo, skip, limit).await?)
}

#[tauri::command]
async fn git_commit_files(
    state: State<'_, AppState>,
    repo: String,
    hash: String,
) -> Result<Vec<Change>, CmdError> {
    let conn = state.current()?;
    Ok(git::commit_files(&conn, &repo, &hash).await?)
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
    repo: String,
    path: String,
    old_path: Option<String>,
    rev: Option<String>,
) -> Result<DiffOut, CmdError> {
    let conn = state.current()?;
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
}

/// Save `text`. With `expected_hash` the save is refused (kind "conflict") if
/// the file changed on the remote since it was read.
#[tauri::command]
async fn write_file(
    state: State<'_, AppState>,
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
    match state.current()?.call(op).await? {
        Reply::Written { hash, stat } => Ok(WrittenOut {
            hash: format!("{hash:016x}"),
            size: stat.size,
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

/// The UI reports whether any tab has unsaved edits.
#[tauri::command]
fn set_unsaved(state: State<'_, AppState>, unsaved: bool) {
    state.unsaved.store(unsaved, Ordering::Relaxed);
}

/// Quit now: the person already decided about unsaved edits.
#[tauri::command]
fn quit_app(app: AppHandle, state: State<'_, AppState>) {
    state.unsaved.store(false, Ordering::Relaxed);
    if let Some(conn) = state.conn.lock().unwrap().take() {
        conn.close();
    }
    app.exit(0);
}

/// Quit, or with unsaved edits ask the UI to confirm first.
fn request_quit(app: &AppHandle) -> bool {
    if app.state::<AppState>().unsaved.load(Ordering::Relaxed) {
        let _ = app.emit("quit-requested", ());
        false
    } else {
        true
    }
}

/// The standard macOS menus, except that Quit goes through `request_quit`.
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
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize()
        .separator()
        .close_window()
        .build()?;
    MenuBuilder::new(app)
        .items(&[&app_menu, &edit, &window])
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
            set_unsaved,
            quit_app,
            askpass_answer
        ])
        .setup(|app| {
            start_askpass(app.handle());
            Ok(())
        })
        .menu(build_menu)
        .on_menu_event(|app, event| {
            if event.id() == "quit" && request_quit(app) {
                app.exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while starting Yonder")
        .run(|app, event| {
            // Quitting from the Dock, or the last window closing: with
            // unsaved edits, ask first. `code` is set when we exit on purpose.
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
