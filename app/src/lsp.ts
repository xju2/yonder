// Language servers on the remote: go to definition, hover, problems and the
// outline for Python (pyright, basedpyright or pylsp) and C/C++ (clangd).
// Whatever the remote has is used, the project's venv first; with none, the
// editor works as before. Each server runs in the workspace folder and ends
// with the connection.

import * as monaco from "monaco-editor";
import { procOpen, streamClose, streamWrite } from "./api";
import type { Place } from "./editor";
import { takeFrames } from "./frames";

interface Kind {
  name: string;
  languages: string[];
  /** sh, run in the workspace folder; exits 127 if no server is found. */
  script: string;
}

/** The project's venv goes first on PATH, so its server and Python are used. */
const VENV = `for v in .venv venv env; do [ -x "$v/bin/python" ] && PATH="$PWD/$v/bin:$PATH" && break; done`;

const KINDS: Kind[] = [
  {
    name: "Python",
    languages: ["python"],
    script: `${VENV}
for c in basedpyright-langserver pyright-langserver; do
  command -v "$c" >/dev/null 2>&1 && exec "$c" --stdio
done
command -v pylsp >/dev/null 2>&1 && exec pylsp
exit 127`,
  },
  {
    name: "C/C++",
    languages: ["cpp", "c"],
    // No background index: it would write .cache/clangd into the project
    // and keep a login node's cores busy.
    script: `command -v clangd >/dev/null 2>&1 && exec clangd --background-index=false
exit 127`,
  },
];

const kindOf = (lang: string) => KINDS.find((k) => k.languages.includes(lang)) ?? null;

const fileUri = (path: string) => monaco.Uri.from({ scheme: "file", path }).toString();
const pathOf = (uri: string) => monaco.Uri.parse(uri).path;

type Pos = { line: number; character: number };
type Range = { start: Pos; end: Pos };
const toRange = (r: Range) =>
  new monaco.Range(r.start.line + 1, r.start.character + 1, r.end.line + 1, r.end.character + 1);
const toPos = (p: monaco.Position): Pos => ({ line: p.lineNumber - 1, character: p.column - 1 });

class Server {
  private buf: Uint8Array = new Uint8Array(0);
  private seq = 0;
  private waiting = new Map<number, { ok: (v: any) => void; fail: (e: Error) => void }>();
  private id: number | null = null;
  private stderr = "";
  ready: Promise<void>;
  dead = false;
  /** It answered `initialize`. */
  started = false;
  /** Documents sent to the server, with a pending change timer each. */
  docs = new Map<monaco.editor.ITextModel, number>();

  constructor(
    readonly conn: number,
    readonly root: string,
    readonly kind: Kind,
    private onDiagnostics: (uri: string, diags: any[]) => void,
    private onExit: (server: Server, missing: boolean, why: string) => void,
  ) {
    this.ready = this.start();
  }

  private async start() {
    this.id = await procOpen(
      this.conn,
      this.root,
      this.kind.script,
      (data, stderr) => {
        if (stderr) this.stderr = (this.stderr + new TextDecoder().decode(data)).slice(-2000);
        else this.receive(data);
      },
      (code) => {
        this.dead = true;
        for (const w of this.waiting.values()) w.fail(new Error("language server exited"));
        this.waiting.clear();
        this.onExit(this, code === 127, this.stderr.trim().split("\n").pop() ?? "");
      },
    );
    await this.request("initialize", {
      processId: null,
      rootUri: fileUri(this.root),
      workspaceFolders: [{ uri: fileUri(this.root), name: this.root.split("/").pop() }],
      capabilities: {
        textDocument: {
          synchronization: { dynamicRegistration: false },
          definition: { linkSupport: false },
          hover: { contentFormat: ["markdown", "plaintext"] },
          documentSymbol: { hierarchicalDocumentSymbolSupport: true },
          publishDiagnostics: {},
        },
        workspace: { configuration: true, workspaceFolders: true },
      },
    });
    this.notify("initialized", {});
    this.started = true;
  }

  private receive(data: Uint8Array) {
    const joined = new Uint8Array(this.buf.length + data.length);
    joined.set(this.buf);
    joined.set(data, this.buf.length);
    const { bodies, rest } = takeFrames(joined);
    this.buf = rest;
    for (const body of bodies) {
      try {
        this.handle(JSON.parse(body));
      } catch (e) {
        console.warn("lsp: bad message", e);
      }
    }
  }

  private handle(msg: any) {
    if (msg.id !== undefined && msg.method === undefined) {
      const w = this.waiting.get(msg.id);
      this.waiting.delete(msg.id);
      if (msg.error) w?.fail(new Error(msg.error.message));
      else w?.ok(msg.result);
    } else if (msg.method === "textDocument/publishDiagnostics") {
      this.onDiagnostics(msg.params.uri, msg.params.diagnostics);
    } else if (msg.id !== undefined) {
      // Requests from the server: configuration gets defaults, the rest
      // (progress, registrations) a plain acknowledgement.
      const result = msg.method === "workspace/configuration" ? msg.params.items.map(() => null) : null;
      this.send({ jsonrpc: "2.0", id: msg.id, result });
    }
  }

  private send(msg: object) {
    if (this.id === null || this.dead) return;
    const body = JSON.stringify(msg);
    const len = new TextEncoder().encode(body).length;
    void streamWrite(this.conn, this.id, `Content-Length: ${len}\r\n\r\n${body}`).catch(() => {});
  }

  notify(method: string, params: object) {
    this.send({ jsonrpc: "2.0", method, params });
  }

  request<T = any>(method: string, params: object): Promise<T> {
    if (this.dead) return Promise.reject(new Error("language server exited"));
    const id = ++this.seq;
    return new Promise<T>((ok, fail) => {
      this.waiting.set(id, { ok, fail });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  close() {
    this.dead = true;
    if (this.id !== null) void streamClose(this.conn, this.id).catch(() => {});
  }
}

export class Languages {
  private servers = new Map<string, Server>();
  /** Servers that are not installed on that host, for this session. */
  private missing = new Set<string>();
  /** The workspace each document belongs to: host and folder. */
  private owner = new Map<monaco.editor.ITextModel, { host: string; root: string }>();

  constructor(
    private workspace: () => { host: string; root: string } | null,
    /** The host's live connection; it changes when reconnecting. */
    private connOf: (host: string) => number | null,
    private open: (path: string, at: Place) => void,
    private status: (msg: string) => void,
  ) {
    monaco.editor.onDidCreateModel((m) => {
      if (m.uri.scheme !== "yonder") return;
      const ws = this.workspace();
      if (!ws) return;
      this.owner.set(m, ws);
      void this.attach(m);
      m.onDidChangeLanguage(() => void this.attach(m));
      m.onDidChangeContent(() => this.changed(m));
    });
    monaco.editor.onWillDisposeModel((m) => {
      for (const s of this.servers.values()) {
        if (!s.docs.has(m)) continue;
        clearTimeout(s.docs.get(m));
        s.docs.delete(m);
        s.notify("textDocument/didClose", { textDocument: { uri: fileUri(m.uri.path) } });
      }
      this.owner.delete(m);
    });
    // Go to Definition into another file opens it in a tab.
    monaco.editor.registerEditorOpener({
      openCodeEditor: (_src, uri, sel) => {
        if (uri.scheme !== "file") return false;
        const at =
          sel && "startLineNumber" in sel
            ? { line: sel.startLineNumber, column: sel.startColumn, length: 0 }
            : sel
              ? { line: sel.lineNumber, column: sel.column, length: 0 }
              : { line: 1, column: 1, length: 0 };
        this.open(uri.path, at);
        return true;
      },
    });
    const langs = KINDS.flatMap((k) => k.languages);
    monaco.languages.registerDefinitionProvider(langs, {
      provideDefinition: async (model, pos) => {
        const r = await this.ask<any>(model, "textDocument/definition", {
          textDocument: { uri: fileUri(model.uri.path) },
          position: toPos(pos),
        });
        const list = !r ? [] : Array.isArray(r) ? r : [r];
        return list.map((l: any) => {
          const uri = l.targetUri ?? l.uri;
          const range = toRange(l.targetSelectionRange ?? l.range);
          const path = pathOf(uri);
          // The same file is this model; others open through the opener.
          return { uri: path === model.uri.path ? model.uri : monaco.Uri.from({ scheme: "file", path }), range };
        });
      },
    });
    monaco.languages.registerHoverProvider(langs, {
      provideHover: async (model, pos) => {
        const r = await this.ask<any>(model, "textDocument/hover", {
          textDocument: { uri: fileUri(model.uri.path) },
          position: toPos(pos),
        });
        if (!r?.contents) return null;
        const parts = Array.isArray(r.contents) ? r.contents : [r.contents];
        const contents = parts.map((c: any) =>
          typeof c === "string"
            ? { value: c }
            : c.kind
              ? { value: c.kind === "plaintext" ? "```\n" + c.value + "\n```" : c.value }
              : { value: "```" + (c.language ?? "") + "\n" + c.value + "\n```" },
        );
        return { contents, range: r.range ? toRange(r.range) : undefined };
      },
    });
    monaco.languages.registerDocumentSymbolProvider(langs, {
      provideDocumentSymbols: async (model) => {
        const r = await this.ask<any[]>(model, "textDocument/documentSymbol", {
          textDocument: { uri: fileUri(model.uri.path) },
        });
        const conv = (s: any): monaco.languages.DocumentSymbol => ({
          name: s.name,
          detail: s.detail ?? "",
          kind: (s.kind ?? 13) - 1,
          tags: [],
          range: toRange(s.range ?? s.location.range),
          selectionRange: toRange(s.selectionRange ?? s.range ?? s.location.range),
          children: (s.children ?? []).map(conv),
        });
        return (r ?? []).map(conv);
      },
    });
  }

  /** Send any typed-but-unsent change, then ask. Null when there is no server. */
  private async ask<T>(model: monaco.editor.ITextModel, method: string, params: object): Promise<T | null> {
    const s = await this.attach(model);
    if (!s) return null;
    this.flush(s, model);
    try {
      return await s.request<T>(method, params);
    } catch {
      return null;
    }
  }

  /** The model's server, started and told about the model if need be. */
  private async attach(model: monaco.editor.ITextModel): Promise<Server | null> {
    const ws = this.owner.get(model);
    const kind = kindOf(model.getLanguageId());
    const conn = ws && this.connOf(ws.host);
    if (!ws || !kind || !conn || model.isDisposed()) return null;
    const key = `${conn}:${ws.root}:${kind.name}`;
    if (this.missing.has(`${conn}:${kind.name}`)) return null;
    let s = this.servers.get(key);
    if (!s || s.dead) {
      s = new Server(
        conn,
        ws.root,
        kind,
        (uri, diags) => this.diagnostics(uri, diags),
        (srv, missing, why) => this.exited(srv, key, missing, why),
      );
      this.servers.set(key, s);
    }
    try {
      await s.ready;
    } catch {
      s.close();
      if (this.servers.get(key) === s) this.servers.delete(key);
      return null;
    }
    if (s.dead || model.isDisposed()) return null;
    if (!s.docs.has(model)) {
      s.docs.set(model, 0);
      s.notify("textDocument/didOpen", {
        textDocument: {
          uri: fileUri(model.uri.path),
          languageId: model.getLanguageId(),
          version: model.getVersionId(),
          text: model.getValue(),
        },
      });
    }
    return s;
  }

  /**
   * Typing: the whole text goes after a pause, which is simple and cheap
   * enough for files the editor opens (16 MB at most). A document whose
   * server stopped (a dropped connection) starts it again.
   */
  private changed(model: monaco.editor.ITextModel) {
    const s = [...this.servers.values()].find((x) => x.docs.has(model) && !x.dead);
    if (!s) return void this.attach(model);
    clearTimeout(s.docs.get(model));
    s.docs.set(model, window.setTimeout(() => this.flush(s, model), 400));
  }

  private flush(s: Server, model: monaco.editor.ITextModel) {
    const timer = s.docs.get(model);
    if (!timer) return;
    clearTimeout(timer);
    s.docs.set(model, 0);
    s.notify("textDocument/didChange", {
      textDocument: { uri: fileUri(model.uri.path), version: model.getVersionId() },
      contentChanges: [{ text: model.getValue() }],
    });
  }

  private diagnostics(uri: string, diags: any[]) {
    const path = pathOf(uri);
    const severity = [0, monaco.MarkerSeverity.Error, monaco.MarkerSeverity.Warning, monaco.MarkerSeverity.Info, monaco.MarkerSeverity.Hint];
    for (const m of monaco.editor.getModels()) {
      if (m.uri.scheme !== "yonder" || m.uri.path !== path) continue;
      monaco.editor.setModelMarkers(
        m,
        "lsp",
        diags.map((d) => ({
          startLineNumber: d.range.start.line + 1,
          startColumn: d.range.start.character + 1,
          endLineNumber: d.range.end.line + 1,
          endColumn: d.range.end.character + 1,
          message: d.message,
          severity: severity[d.severity ?? 1] ?? monaco.MarkerSeverity.Error,
          source: d.source,
          code: d.code === undefined ? undefined : String(d.code),
        })),
      );
    }
  }

  private exited(s: Server, key: string, missing: boolean, why: string) {
    if (this.servers.get(key) === s) this.servers.delete(key);
    for (const [m, timer] of s.docs) {
      clearTimeout(timer);
      if (!m.isDisposed()) monaco.editor.setModelMarkers(m, "lsp", []);
    }
    s.docs.clear();
    // One that never started will not start next time either.
    if (!missing && !s.started) {
      this.missing.add(`${s.conn}:${s.kind.name}`);
      this.status(`${s.kind.name} language server did not start${why ? `: ${why}` : "."}`);
    } else if (missing) {
      this.missing.add(`${s.conn}:${s.kind.name}`);
      this.status(
        s.kind.name === "Python"
          ? "No Python language server on the remote; pip install basedpyright (in the venv) for Go to Definition."
          : "No clangd on the remote; C/C++ Go to Definition needs it on PATH.",
      );
    }
    // Open documents restart the server on their next request.
  }
}
