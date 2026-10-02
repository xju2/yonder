// Editor tabs on top of Monaco. Every open file is a local Monaco model, so
// typing never touches the network; only saving does.

import * as monaco from "monaco-editor";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import JsonWorker from "monaco-editor/language/json/json.worker?worker";
import CssWorker from "monaco-editor/language/css/css.worker?worker";
import HtmlWorker from "monaco-editor/language/html/html.worker?worker";
import TsWorker from "monaco-editor/language/typescript/ts.worker?worker";
import { asError, readBytes, readFile, stat, writeFile } from "./api";
import { ask, tell } from "./modal";
import { createViewer, viewKindFor, type ViewKind, type Viewer } from "./viewer";

self.MonacoEnvironment = {
  getWorker(_id: string, label: string) {
    switch (label) {
      case "json":
        return new JsonWorker();
      case "css":
      case "scss":
      case "less":
        return new CssWorker();
      case "html":
      case "handlebars":
      case "razor":
        return new HtmlWorker();
      case "typescript":
      case "javascript":
        return new TsWorker();
      default:
        return new EditorWorker();
    }
  },
};

// Without the rest of the project the TypeScript service would flag every
// import as missing; keep syntax errors only.
for (const d of [monaco.typescript.typescriptDefaults, monaco.typescript.javascriptDefaults]) {
  d.setDiagnosticsOptions({ noSemanticValidation: true, noSyntaxValidation: false });
}

const dark = window.matchMedia("(prefers-color-scheme: dark)");
const applyTheme = () => monaco.editor.setTheme(dark.matches ? "vs-dark" : "vs");
dark.addEventListener("change", applyTheme);

function languageFor(path: string): string {
  const name = path.split("/").pop()!.toLowerCase();
  for (const lang of monaco.languages.getLanguages()) {
    if (lang.filenames?.some((f) => f.toLowerCase() === name)) return lang.id;
  }
  let best = "plaintext";
  let bestLen = 0;
  for (const lang of monaco.languages.getLanguages()) {
    for (const ext of lang.extensions ?? []) {
      if (name.endsWith(ext.toLowerCase()) && ext.length > bestLen) {
        best = lang.id;
        bestLen = ext.length;
      }
    }
  }
  return best;
}

export const baseName = (path: string) => path.split("/").pop() || path;

interface Tab {
  path: string;
  /** null for files that cannot be shown as text. */
  model: monaco.editor.ITextModel | null;
  note: string;
  /** Hash of the remote content this tab is based on. */
  hash: string | null;
  savedVersion: number;
  view: monaco.editor.ICodeEditorViewState | null;
  el: HTMLElement;
  /** Image or PDF viewer, for files that are shown rather than edited. */
  viewer: Viewer | null;
  /** The remote file's size and mtime when the viewer last loaded it. */
  version: string | null;
}

export class Editors {
  private tabs: Tab[] = [];
  private active: Tab | null = null;
  private editor: monaco.editor.IStandaloneCodeEditor;

  constructor(
    private host: HTMLElement,
    private viewerHost: HTMLElement,
    private tabsEl: HTMLElement,
    private placeholder: HTMLElement,
    private status: (msg: string) => void,
    private onActive: (path: string | null) => void,
    private position: (text: string) => void,
  ) {
    this.editor = monaco.editor.create(host, {
      model: null,
      automaticLayout: true,
      fontFamily: '"SF Mono", Menlo, Monaco, "DejaVu Sans Mono", monospace',
      fontSize: 13,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      renderWhitespace: "selection",
    });
    applyTheme();
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => void this.save());
    this.editor.onDidChangeCursorPosition((e) =>
      this.position(`Ln ${e.position.lineNumber}, Col ${e.position.column}`),
    );
    this.show(null);
  }

  hasUnsaved(): boolean {
    return this.tabs.some((t) => this.isDirty(t));
  }

  async open(path: string) {
    const existing = this.tabs.find((t) => t.path === path);
    if (existing) return this.show(existing);
    const kind = viewKindFor(path);
    if (kind) return this.openViewer(path, kind);
    this.status(`Opening ${baseName(path)}…`);
    let file;
    try {
      file = await readFile(path);
    } catch (e) {
      const err = asError(e);
      this.status("");
      if (err.kind === "too_large") {
        await tell(`${baseName(path)} is too large to open in the editor.`, err.message);
      } else {
        await tell(`Could not open ${baseName(path)}.`, err.message);
      }
      return;
    }
    // Another click may have opened it while we waited.
    const raced = this.tabs.find((t) => t.path === path);
    if (raced) return this.show(raced);

    const tab: Tab = {
      path,
      model: null,
      note: "",
      hash: file.hash,
      savedVersion: 0,
      view: null,
      el: document.createElement("div"),
      viewer: null,
      version: null,
    };
    if (file.text === null) {
      tab.note = `${baseName(path)} is not a text file (${file.size.toLocaleString()} bytes).`;
    } else {
      const uri = monaco.Uri.from({ scheme: "yonder", path });
      monaco.editor.getModel(uri)?.dispose();
      tab.model = monaco.editor.createModel(file.text, languageFor(path), uri);
      tab.savedVersion = tab.model.getAlternativeVersionId();
      tab.model.onDidChangeContent(() => this.paintTab(tab));
    }
    this.buildTab(tab);
    this.tabs.push(tab);
    this.status("");
    this.show(tab);
  }

  private async openViewer(path: string, kind: ViewKind) {
    const name = baseName(path);
    this.status(`Opening ${name}…`);
    let version: string;
    let bytes: ArrayBuffer;
    try {
      version = (await stat(path)).version;
      bytes = await readBytes(path);
    } catch (e) {
      const err = asError(e);
      this.status("");
      const what = err.kind === "too_large" ? `${name} is too large to view.` : `Could not open ${name}.`;
      await tell(what, err.message);
      return;
    }
    const raced = this.tabs.find((t) => t.path === path);
    if (raced) return this.show(raced);

    const tab: Tab = {
      path,
      model: null,
      note: "",
      hash: null,
      savedVersion: 0,
      view: null,
      el: document.createElement("div"),
      viewer: null,
      version,
    };
    tab.viewer = createViewer(kind, path, () => void this.refreshViewer(tab, true));
    this.buildTab(tab);
    this.tabs.push(tab);
    // Show first: fitting a PDF to the window needs the window's size.
    this.show(tab);
    try {
      await tab.viewer.load(bytes);
      this.status("");
      if (this.active === tab) this.position(tab.viewer.info());
    } catch (e) {
      tab.viewer.dispose();
      tab.viewer = null;
      tab.note = `Could not show ${name}: ${e instanceof Error ? e.message : String(e)}`;
      this.status("");
      if (this.active === tab) this.show(tab);
    }
  }

  /** Re-check the active image or PDF and reload it if it changed on the remote. */
  refreshActive() {
    if (this.active?.viewer) void this.refreshViewer(this.active);
  }

  private async refreshViewer(tab: Tab, force = false) {
    const viewer = tab.viewer;
    if (!viewer) return;
    const name = baseName(tab.path);
    try {
      const s = await stat(tab.path);
      if (!force && s.version === tab.version) return;
      const bytes = await readBytes(tab.path);
      await viewer.load(bytes);
      tab.version = s.version;
      if (this.active === tab) this.position(viewer.info());
      this.status(`Reloaded ${name}`);
    } catch (e) {
      const err = asError(e);
      if (err.kind === "not_found") this.status(`${name} no longer exists on the remote.`);
      else if (err.kind !== "disconnected") this.status(`Could not reload ${name}: ${err.message}`);
    }
  }

  /** Save the active tab, resolving conflicts with the person. */
  async save(tab = this.active) {
    if (!tab?.model) return;
    const model = tab.model;
    const text = model.getValue();
    const version = model.getAlternativeVersionId();
    const name = baseName(tab.path);
    let expected = tab.hash;
    for (;;) {
      try {
        const w = await writeFile(tab.path, text, expected);
        tab.hash = w.hash;
        tab.savedVersion = version;
        this.paintTab(tab);
        this.status(`Saved ${name}`);
        return;
      } catch (e) {
        const err = asError(e);
        if (err.kind === "conflict") {
          const choice = await ask(
            `${name} changed on the remote since you opened it.`,
            [
              { value: "cancel", label: "Cancel", primary: true },
              { value: "reload", label: "Discard my edits and reload" },
              { value: "overwrite", label: "Overwrite the remote file", danger: true },
            ],
          );
          if (choice === "overwrite") {
            expected = null;
            continue;
          }
          if (choice === "reload") await this.reload(tab);
          return;
        }
        if (err.kind === "disconnected") {
          this.status(`Not saved: not connected. Your edits are kept; reconnect to save.`);
          return;
        }
        await tell(`Could not save ${name}.`, err.message);
        return;
      }
    }
  }

  /** Replace the tab's content with what is on the remote now. */
  private async reload(tab: Tab) {
    try {
      const file = await readFile(tab.path);
      if (!tab.model || file.text === null) return;
      tab.model.setValue(file.text);
      tab.hash = file.hash;
      tab.savedVersion = tab.model.getAlternativeVersionId();
      this.paintTab(tab);
      this.status(`Reloaded ${baseName(tab.path)}`);
    } catch (e) {
      await tell(`Could not reload ${baseName(tab.path)}.`, asError(e).message);
    }
  }

  async close(tab: Tab) {
    if (this.isDirty(tab)) {
      const choice = await ask(`Save changes to ${baseName(tab.path)}?`, [
        { value: "save", label: "Save", primary: true },
        { value: "discard", label: "Don't save", danger: true },
        { value: "cancel", label: "Cancel" },
      ]);
      if (choice === "cancel") return;
      if (choice === "save") {
        await this.save(tab);
        if (this.isDirty(tab)) return;
      }
    }
    const i = this.tabs.indexOf(tab);
    this.tabs.splice(i, 1);
    tab.el.remove();
    tab.model?.dispose();
    tab.viewer?.dispose();
    if (this.active === tab) this.show(this.tabs[Math.min(i, this.tabs.length - 1)] ?? null);
  }

  private isDirty(t: Tab) {
    return t.model !== null && t.model.getAlternativeVersionId() !== t.savedVersion;
  }

  private buildTab(tab: Tab) {
    tab.el.className = "tab";
    tab.el.title = tab.path;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = baseName(tab.path);
    const close = document.createElement("button");
    close.className = "close";
    close.title = "Close";
    close.textContent = "×";
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.close(tab);
    });
    tab.el.append(name, close);
    tab.el.addEventListener("click", () => this.show(tab));
    tab.el.addEventListener("auxclick", (e) => {
      if (e.button === 1) void this.close(tab);
    });
    this.tabsEl.append(tab.el);
  }

  private paintTab(tab: Tab) {
    tab.el.classList.toggle("dirty", this.isDirty(tab));
    tab.el.classList.toggle("active", tab === this.active);
  }

  private show(tab: Tab | null) {
    if (this.active?.model) this.active.view = this.editor.saveViewState();
    this.active = tab;
    this.tabs.forEach((t) => this.paintTab(t));
    const hasText = !!tab?.model;
    const viewer = tab?.viewer ?? null;
    this.host.style.visibility = hasText ? "visible" : "hidden";
    this.host.style.display = viewer ? "none" : "";
    this.viewerHost.hidden = !viewer;
    if (viewer) this.viewerHost.replaceChildren(viewer.el);
    else this.viewerHost.replaceChildren();
    this.placeholder.hidden = hasText || !!viewer;
    this.placeholder.textContent = tab ? tab.note : "Open a file from the tree.";
    this.editor.setModel(tab?.model ?? null);
    if (tab?.model) {
      if (tab.view) this.editor.restoreViewState(tab.view);
      this.editor.focus();
    }
    if (viewer) {
      this.position(viewer.info());
      // A plot may have been regenerated while another tab was showing.
      void this.refreshViewer(tab!);
    }
    tab?.el.scrollIntoView({ block: "nearest", inline: "nearest" });
    this.onActive(tab?.path ?? null);
  }
}
