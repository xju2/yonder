//! Yonder's desktop process: owns the connection and exposes it to the UI as
//! Tauri commands. All editing state lives in the UI; this side only moves
//! bytes, so keystrokes never wait on it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{ipc::Response, AppHandle, Emitter, Manager, State};
use yonder_client::{connect as open_connection, ConnectOptions, Connection, Level, LogLine};
use yonder_proto::{EntryKind, Error as ProtoError, ErrorKind, Op, Reply};

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
    let opts = ConnectOptions::new(host.clone(), agent_dirs(&app));
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

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            connect, disconnect, list_dir, read_file, read_bytes, stat, write_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running Yonder");
}
