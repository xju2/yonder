// Typed wrappers around the Tauri commands in src-tauri/src/main.rs.

import { invoke } from "@tauri-apps/api/core";
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
}

export interface Written {
  hash: string;
  size: number;
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

export const connect = (host: string, path: string) =>
  invoke<ConnInfo>("connect", { host, path });
export const disconnect = () => invoke<void>("disconnect");
export const listDir = (path: string) => invoke<Entry[]>("list_dir", { path });
export const readFile = (path: string) => invoke<FileContent>("read_file", { path });
/** Raw bytes, for the image and PDF viewers. */
export const readBytes = (path: string) => invoke<ArrayBuffer>("read_bytes", { path });

export interface Stat {
  size: number;
  /** Changes whenever the file's size or modification time does. */
  version: string;
}
export const stat = (path: string) => invoke<Stat>("stat", { path });

export const writeFile = (path: string, text: string, expectedHash: string | null) =>
  invoke<Written>("write_file", { path, text, expectedHash });

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
