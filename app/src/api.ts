// Typed wrappers around the Tauri commands in src-tauri/src/main.rs.

import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ConnInfo {
  generation: number;
  host: string;
  root: string;
  hostname: string;
  home: string;
}

export interface Entry {
  name: string;
  kind: "file" | "dir" | "broken" | "other";
  symlink: boolean;
  size: number;
}

export interface FileContent {
  /** null when the file is not UTF-8 text. */
  text: string | null;
  hash: string;
  size: number;
  /** As Stat.version: whether the file changed since. */
  version: string;
}

export interface Written {
  hash: string;
  size: number;
  version: string;
}

export type ErrorKind =
  | "connect"
  | "disconnected"
  | "not_found"
  | "permission_denied"
  | "is_directory"
  | "not_directory"
  | "too_large"
  | "conflict"
  | "other";

export interface CmdError {
  kind: ErrorKind;
  message: string;
  hint: string | null;
}

/** Normalise anything thrown by `invoke` into a CmdError. */
export function asError(e: unknown): CmdError {
  if (e && typeof e === "object" && "kind" in e && "message" in e) return e as CmdError;
  return { kind: "other", message: String(e), hint: null };
}

/**
 * The connection that calls go to: the open workspace's. Each call takes it
 * when made, so one that started before a switch stays with its host.
 */
let conn = 0;
export const useConnection = (id: number) => (conn = id);
export const currentConnection = () => conn;

export const connect = (host: string, path: string) =>
  invoke<ConnInfo>("connect", { host, path });
/** `local` (this Mac), then the Host aliases in ~/.ssh/config. */
export const sshHosts = () => invoke<string[]>("ssh_hosts");
/** Resolve another folder on the connected host; no new ssh session. */
export const openFolder = (id: number, path: string) => invoke<string>("open_folder", { conn: id, path });
export const disconnect = (id: number) => invoke<void>("disconnect", { conn: id });
export const listDir = (path: string) => invoke<Entry[]>("list_dir", { conn, path });
export const readFile = (path: string) => invoke<FileContent>("read_file", { conn, path });
/** Raw bytes, for the image and PDF viewers. */
export const readBytes = (path: string) => invoke<ArrayBuffer>("read_bytes", { conn, path });

export interface Stat {
  size: number;
  /** Changes whenever the file's size or modification time does. */
  version: string;
}
export const stat = (path: string) => invoke<Stat>("stat", { conn, path });

export const writeFile = (path: string, text: string, expectedHash: string | null) =>
  invoke<Written>("write_file", { conn, path, text, expectedHash });

export interface LogEvent {
  level: "step" | "warn" | "remote";
  message: string;
}
export interface ClosedEvent {
  generation: number;
  reason: string;
}

export const onLog = (f: (e: LogEvent) => void): Promise<UnlistenFn> =>
  listen<LogEvent>("conn-log", (e) => f(e.payload));
export const onClosed = (f: (e: ClosedEvent) => void): Promise<UnlistenFn> =>
  listen<ClosedEvent>("conn-closed", (e) => f(e.payload));

// ---- terminals

export interface PtyOpened {
  pty: number;
  /** The shell exited before this call returned (no pty-exit event follows). */
  exited: boolean;
  code: number | null;
}

/** Start the remote login shell; its output streams to `output`. */
export const ptyOpen = (
  conn: number,
  cols: number,
  rows: number,
  cwd: string | null,
  output: Channel<ArrayBuffer>,
) => invoke<PtyOpened>("pty_open", { conn, cols, rows, cwd, output });
/** Keystrokes. Calls reach the shell in the order they are made. */
export const ptyWrite = (conn: number, pty: number, data: string) =>
  invoke<void>("pty_write", { conn, pty, data });
/** The terminal has drawn `bytes` more output, so the remote may send more. */
export const ptyAck = (conn: number, pty: number, bytes: number) =>
  invoke<void>("pty_ack", { conn, pty, bytes });
export const ptyResize = (conn: number, pty: number, cols: number, rows: number) =>
  invoke<void>("pty_resize", { conn, pty, cols, rows });
export const ptyClose = (conn: number, pty: number) => invoke<void>("pty_close", { conn, pty });

export interface PtyExitEvent {
  conn: number;
  pty: number;
  /** null when a signal ended the shell. */
  code: number | null;
}
export const onPtyExit = (f: (e: PtyExitEvent) => void): Promise<UnlistenFn> =>
  listen<PtyExitEvent>("pty-exit", (e) => f(e.payload));

// ---- remote processes and tunnels

/**
 * Run `script` with sh in `cwd`, from the login shell. `onExit` gets the
 * exit code, or null after a signal or a lost connection.
 */
export function procOpen(
  conn: number,
  cwd: string,
  script: string,
  onOutput: (data: Uint8Array, stderr: boolean) => void,
  onExit: (code: number | null) => void,
): Promise<number> {
  // Each message is a tag byte then data: 1 stdout, 2 stderr, 0 the end
  // (then the exit code in decimal, if any).
  const output = new Channel<ArrayBuffer>();
  output.onmessage = (buf) => {
    const bytes = new Uint8Array(buf);
    if (bytes[0] === 0) {
      const code = new TextDecoder().decode(bytes.subarray(1));
      onExit(code ? Number(code) : null);
    } else onOutput(bytes.subarray(1), bytes[0] === 2);
  };
  return invoke<number>("proc_open", { conn, cwd, script, output });
}
/** Bytes for the process's stdin, in call order. */
export const streamWrite = (conn: number, id: number, data: string) =>
  invoke<void>("stream_write", { conn, id, data });
export const streamClose = (conn: number, id: number) => invoke<void>("stream_close", { conn, id });
/** A port on this Mac leading to `port` on the remote's loopback. */
export const tunnelOpen = (conn: number, port: number) => invoke<number>("tunnel_open", { conn, port });

// ---- git (read-only)

export interface GitChange {
  /** Relative to the repository root. */
  path: string;
  old_path: string | null;
  /** M, A, D, R, C, U (conflict) or ? (untracked). */
  status: string;
}
export interface GitStatus {
  branch: string | null;
  upstream: string | null;
  ahead: number;
  behind: number;
  files: GitChange[];
  /** Relative to the repository root; ignored folders end in "/". */
  ignored: string[];
}
export interface GitCommit {
  hash: string;
  short: string;
  author: string;
  /** Seconds since the epoch. */
  time: number;
  subject: string;
  merge: boolean;
}
export interface GitDiff {
  /** null when the file is not text on either side. */
  original: string | null;
  modified: string | null;
}

/** `repo` is null when `dir` is not inside a git repository. */
export const gitStatus = (dir: string) =>
  invoke<{ repo: string | null; status: GitStatus | null }>("git_status", { conn, dir });
export const gitLog = (repo: string, skip: number, limit: number) =>
  invoke<GitCommit[]>("git_log", { conn, repo, skip, limit });
export const gitCommitFiles = (repo: string, hash: string) =>
  invoke<GitChange[]>("git_commit_files", { conn, repo, hash });
/** Without `rev`: HEAD against the working tree. With it: that commit against its parent. */
export const gitDiff = (repo: string, path: string, oldPath: string | null, rev: string | null) =>
  invoke<GitDiff>("git_diff", { conn, repo, path, oldPath, rev });

/** Files under `dir` that git does not ignore, relative to it; null outside a repository. */
export const gitFiles = (dir: string) => invoke<string[] | null>("git_files", { conn, dir });

export interface SearchMatch {
  /** Relative to the searched folder. */
  path: string;
  line: number;
  text: string;
}
export interface SearchQuery {
  pattern: string;
  regex: boolean;
  caseSensitive: boolean;
  word: boolean;
  /** Globs as in VS Code: `*.py` and `build` match at any depth, `./src` from `dir`. */
  include: string[];
  exclude: string[];
}
/** Lines matching `query` in the files under `dir` that git does not ignore. */
export const search = (dir: string, query: SearchQuery) =>
  invoke<{ matches: SearchMatch[]; truncated: boolean }>("search", { conn, dir, query });

// ---- quitting

/** Quit without further questions. */
export const quitApp = () => invoke<void>("quit_app");
/** Cmd+Q or the Dock asked to quit while edits are unsaved. */
export const onQuitRequested = (f: () => void): Promise<UnlistenFn> =>
  listen("quit-requested", () => f());

/** Put text on the clipboard. */
export const copyText = (text: string) => invoke<void>("copy_text", { text });
/** Put a PNG image, and only that, on the clipboard. */
export const copyPng = (png: Uint8Array) => invoke<void>("copy_png", png);

/** A File or View menu item (or its shortcut) was chosen. */
export const onMenu = (f: (id: string) => void): Promise<UnlistenFn> =>
  listen<string>("menu", (e) => f(e.payload));

// ---- questions from ssh

export interface AskpassEvent {
  id: number;
  prompt: string;
}
/** ssh asks for a password, a one-time code, or whether to trust a host key. */
export const onAskpass = (f: (e: AskpassEvent) => void): Promise<UnlistenFn> =>
  listen<AskpassEvent>("askpass", (e) => f(e.payload));
/** `answer` null cancels, which makes ssh give up. */
export const askpassAnswer = (id: number, answer: string | null) =>
  invoke<void>("askpass_answer", { id, answer });
