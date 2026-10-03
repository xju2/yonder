// Editor tabs on top of Monaco. Every open file is a local Monaco model, so
// typing never touches the network; only saving does.

import * as monaco from "monaco-editor";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import JsonWorker from "monaco-editor/language/json/json.worker?worker";
import CssWorker from "monaco-editor/language/css/css.worker?worker";
import HtmlWorker from "monaco-editor/language/html/html.worker?worker";
import TsWorker from "monaco-editor/language/typescript/ts.worker?worker";
import { marked } from "marked";
import "./languages";
// The code font, bundled: Zed's default is a variant of IBM Plex Mono.
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/400-italic.css";
import "@fontsource/ibm-plex-mono/700.css";
import "@fontsource/ibm-plex-mono/700-italic.css";
import { asError, gitDiff, readBytes, readFile, stat, writeFile } from "./api";
import type { DiffRequest } from "./git";
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

/** A file-name glob where `*` is any run of characters. */
const globMatch = (glob: string, name: string) =>
  new RegExp(`^${glob.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`).test(name);

function languageFor(path: string): string {
  const name = path.split("/").pop()!.toLowerCase();
  for (const lang of monaco.languages.getLanguages()) {
    if (lang.filenames?.some((f) => f.toLowerCase() === name)) return lang.id;
  }
  for (const lang of monaco.languages.getLanguages()) {
    if (lang.filenamePatterns?.some((p) => globMatch(p.toLowerCase(), name))) return lang.id;
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

/** Markdown preview styles; the preview's frame does not see the app's. */
const PREVIEW_CSS = `
:root { color-scheme: light dark; }
body { font: 14px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  max-width: 820px; margin: 0 auto; padding: 16px 32px 48px; }
pre, code { font: 12.5px "SF Mono", Menlo, Monaco, monospace; background: rgb(127 127 127 / 0.12); border-radius: 4px; }
code { padding: 1px 4px; }
pre { padding: 10px 12px; overflow: auto; }
pre code { padding: 0; background: none; }
img { max-width: 100%; }
table { border-collapse: collapse; }
th, td { border: 1px solid rgb(127 127 127 / 0.35); padding: 4px 10px; }
blockquote { margin: 0; padding-left: 12px; border-left: 3px solid rgb(127 127 127 / 0.4); opacity: 0.85; }
h1, h2 { border-bottom: 1px solid rgb(127 127 127 / 0.25); padding-bottom: 4px; }
`;

const isMarkdown = (path: string) => /\.(md|markdown|mdown|mkd)$/i.test(path);

export const baseName = (path: string) => path.split("/").pop() || path;

/** Where to put the cursor in a file that opens: 1-based, as Monaco counts. */
export interface Place {
  line: number;
  column: number;
  /** Characters to select from there. */
  length: number;
}

/** Each model gets its own URI: two workspaces may open the same path. */
let modelSeq = 0;

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
  /** The viewer's pending load or reload; the next one waits for it. */
  loading: Promise<void> | null;
  /** A read-only comparison, for diff tabs. */
  diff: { original: monaco.editor.ITextModel; modified: monaco.editor.ITextModel } | null;
  /** Tab title when it is not the file name. */
  label: string | null;
  /** Rendered Markdown, shown instead of the text while set. */
  preview: HTMLIFrameElement | null;
}

export class Editors {
  private tabs: Tab[] = [];
  private active: Tab | null = null;
  /** Tabs of the other workspaces, unsaved edits included, by workspace. */
  private kept = new Map<string, { tabs: Tab[]; active: Tab | null }>();
  /** Bumped by each switch; a file still loading for the old one is dropped. */
  private generation = 0;
  private editor: monaco.editor.IStandaloneCodeEditor;

  private diffEditor: monaco.editor.IStandaloneDiffEditor | null = null;

  constructor(
    private host: HTMLElement,
    private viewerHost: HTMLElement,
    private diffHost: HTMLElement,
    private tabsEl: HTMLElement,
    private placeholder: HTMLElement,
    private status: (msg: string) => void,
    private onActive: (path: string | null) => void,
    private position: (text: string) => void,
    private onSaved: () => void,
    private onTabMenu: (path: string, reload: (() => void) | null) => void,
    /** A tab was double-clicked: show its file in the tree. */
    private onReveal: (path: string) => void,
  ) {
    this.editor = monaco.editor.create(host, {
      model: null,
      automaticLayout: true,
      fontFamily: '"IBM Plex Mono", "SF Mono", Menlo, Monaco, "DejaVu Sans Mono", monospace',
      fontSize: 13,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      renderWhitespace: "selection",
    });
    applyTheme();
    // Monaco measures characters once; measure again once the font is in.
    void document.fonts.load('13px "IBM Plex Mono"').then(() => monaco.editor.remeasureFonts());
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => void this.save());
    this.editor.onDidChangeCursorPosition((e) =>
      this.position(`Ln ${e.position.lineNumber}, Col ${e.position.column}`),
    );
    this.show(null);
  }

  hasUnsaved(): boolean {
    const all = [this.tabs, ...[...this.kept.values()].map((k) => k.tabs)].flat();
    return all.some((t) => this.isDirty(t));
  }

  /** Put away this workspace's tabs and bring back those of workspace `key`. */
  switchTo(from: string | null, key: string) {
    this.generation++;
    if (this.active?.model) this.active.view = this.editor.saveViewState();
    if (from) this.kept.set(from, { tabs: this.tabs, active: this.active });
    for (const t of this.tabs) t.el.remove();
    const next = this.kept.get(key) ?? { tabs: [], active: null };
    this.kept.delete(key);
    this.tabs = next.tabs;
    for (const t of this.tabs) this.tabsEl.append(t.el);
    // Its view state is saved above; show() must not save it again.
    this.active = null;
    this.show(next.active);
  }

  /** The selected text, if it is on one line: a search to start with. */
  selection(): string {
    const sel = this.editor.getSelection();
    if (!sel || sel.isEmpty() || sel.startLineNumber !== sel.endLineNumber) return "";
    return this.editor.getModel()?.getValueInRange(sel) ?? "";
  }

  /** Open `path` in a tab, or show its tab; `at` selects a place in it. */
  async open(path: string, at?: Place) {
    await this.openTab(path);
    const tab = this.active;
    if (!at || tab?.path !== path || !tab.model) return;
    const range = new monaco.Range(at.line, at.column, at.line, at.column + at.length);
    this.editor.setSelection(range);
    this.editor.revealRangeInCenter(range);
    this.editor.focus();
  }

  private async openTab(path: string) {
    const existing = this.tabs.find((t) => t.path === path);
    if (existing) return this.show(existing);
    const kind = viewKindFor(path);
    if (kind) return this.openViewer(path, kind);
    this.status(`Opening ${baseName(path)}…`);
    const gen = this.generation;
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
    if (gen !== this.generation) return;
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
      loading: null,
      diff: null,
      label: null,
      preview: null,
    };
    if (file.text === null) {
      tab.note = `${baseName(path)} is not a text file (${file.size.toLocaleString()} bytes).`;
    } else {
      const uri = monaco.Uri.from({ scheme: "yonder", authority: String(++modelSeq), path });
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
    const gen = this.generation;
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
    if (gen !== this.generation) return;
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
      loading: null,
      diff: null,
      label: null,
      preview: null,
    };
    const viewer = createViewer(
      kind,
      path,
      () => void this.refreshViewer(tab, true),
      (msg) => this.status(msg),
    );
    tab.viewer = viewer;
    // Reloads requested while the first load runs wait for it.
    let firstLoaded!: () => void;
    const first = new Promise<void>((resolve) => (firstLoaded = resolve));
    tab.loading = first;
    this.buildTab(tab);
    this.tabs.push(tab);
    // Show first: fitting a PDF to the window needs the window's size.
    this.show(tab);
    try {
      await viewer.load(bytes);
      this.status("");
      if (this.active === tab) this.position(viewer.info());
    } catch (e) {
      viewer.dispose();
      tab.viewer = null;
      tab.note = `Could not show ${name}: ${e instanceof Error ? e.message : String(e)}`;
      this.status("");
      if (this.active === tab) this.show(tab);
    } finally {
      if (tab.loading === first) tab.loading = null;
      firstLoaded();
    }
  }

  /** Open a read-only comparison of one file. */
  async openDiff(req: DiffRequest) {
    const key = `diff:${req.rev ?? "work"}:${req.repo}/${req.path}`;
    const name = baseName(req.path);
    const existing = this.tabs.find((t) => t.path === key);
    // Uncommitted changes may have moved on since the tab was opened.
    if (existing && req.rev) return this.show(existing);
    this.status(`Comparing ${name}…`);
    const gen = this.generation;
    let d;
    try {
      d = await gitDiff(req.repo, req.path, req.oldPath, req.rev);
    } catch (e) {
      this.status("");
      await tell(`Could not compare ${name}.`, asError(e).message);
      return;
    }
    this.status("");
    if (gen !== this.generation) return;
    const lang = languageFor(req.path);
    const raced = this.tabs.find((t) => t.path === key);
    if (raced) {
      this.setDiff(raced, name, lang, d.original, d.modified);
      return this.show(raced);
    }
    const tab: Tab = {
      path: key,
      model: null,
      note: "",
      hash: null,
      savedVersion: 0,
      view: null,
      el: document.createElement("div"),
      viewer: null,
      version: null,
      loading: null,
      diff: null,
      label: req.rev ? `${name} @ ${req.short}` : `${name} (changes)`,
      preview: null,
    };
    this.setDiff(tab, name, lang, d.original, d.modified);
    this.buildTab(tab);
    tab.el.title = req.oldPath ? `${req.oldPath} → ${req.path}` : req.path;
    this.tabs.push(tab);
    this.show(tab);
  }

  /** Give a diff tab new contents; either side null means not text. */
  private setDiff(
    tab: Tab,
    name: string,
    lang: string,
    original: string | null,
    modified: string | null,
  ) {
    if (original === null || modified === null) {
      // Now binary: drop any text comparison shown before.
      this.dropDiff(tab);
      tab.note = `${name} is not a text file, so there is no line-by-line comparison.`;
    } else if (tab.diff) {
      tab.diff.original.setValue(original);
      tab.diff.modified.setValue(modified);
    } else {
      tab.note = "";
      tab.diff = {
        original: monaco.editor.createModel(original, lang),
        modified: monaco.editor.createModel(modified, lang),
      };
    }
  }

  private dropDiff(tab: Tab) {
    if (!tab.diff) return;
    // Detach before disposing, or the diff editor keeps dead models.
    if (this.diffEditor?.getModel()?.original === tab.diff.original) this.diffEditor.setModel(null);
    tab.diff.original.dispose();
    tab.diff.modified.dispose();
    tab.diff = null;
  }

  private getDiffEditor(): monaco.editor.IStandaloneDiffEditor {
    this.diffEditor ??= monaco.editor.createDiffEditor(this.diffHost, {
      automaticLayout: true,
      readOnly: true,
      originalEditable: false,
      fontFamily: '"IBM Plex Mono", "SF Mono", Menlo, Monaco, "DejaVu Sans Mono", monospace',
      fontSize: 13,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      // Narrow windows show the change inline instead of side by side.
      useInlineViewWhenSpaceIsLimited: true,
    });
    return this.diffEditor;
  }

  /** Re-check the active image or PDF and reload it if it changed on the remote. */
  refreshActive() {
    if (this.active?.viewer) void this.refreshViewer(this.active);
  }

  /**
   * Reload the tab's viewer if its file changed (always, with `force`).
   * One load per tab at a time, so an older file never replaces a newer one;
   * a check requested while another is queued joins it.
   */
  private refreshViewer(tab: Tab, force = false): Promise<void> {
    if (tab.loading && !force) return tab.loading;
    const next: Promise<void> = (tab.loading ?? Promise.resolve())
      .then(() => this.reloadIfChanged(tab, force))
      .finally(() => {
        if (tab.loading === next) tab.loading = null;
      });
    tab.loading = next;
    return next;
  }

  private async reloadIfChanged(tab: Tab, force: boolean) {
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
      // The previous content stays on screen; the next check tries again.
      const err = e instanceof Error ? { kind: "other", message: e.message } : asError(e);
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
        this.onSaved();
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
    this.dropDiff(tab);
    if (this.active === tab) this.show(this.tabs[Math.min(i, this.tabs.length - 1)] ?? null);
  }

  /** Switch the active Markdown tab between its text and a rendered view. */
  togglePreview() {
    const tab = this.active;
    if (!tab?.model || !isMarkdown(tab.path)) return;
    if (tab.preview) {
      tab.preview = null;
    } else {
      const frame = document.createElement("iframe");
      frame.className = "md-preview";
      // No scripts and an opaque origin: a remote file's HTML cannot reach
      // the app's commands.
      frame.sandbox.value = "";
      frame.srcdoc = `<!doctype html><meta charset="utf-8"><style>${PREVIEW_CSS}</style>${marked.parse(
        tab.model.getValue(),
        { async: false },
      )}`;
      tab.preview = frame;
    }
    this.show(tab);
  }

  closeActive() {
    if (this.active) void this.close(this.active);
  }

  private isDirty(t: Tab) {
    return t.model !== null && t.model.getAlternativeVersionId() !== t.savedVersion;
  }

  private buildTab(tab: Tab) {
    tab.el.className = "tab";
    tab.el.title = tab.path;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = tab.label ?? baseName(tab.path);
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
    tab.el.addEventListener("dblclick", (e) => {
      if (e.target !== close) this.onReveal(tab.path.replace(/^diff:[^:]*:/, ""));
    });
    tab.el.addEventListener("auxclick", (e) => {
      if (e.button === 1) void this.close(tab);
    });
    tab.el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      // Reload would throw away unsaved edits, so those tabs don't offer it.
      const reload = tab.viewer
        ? () => void this.refreshViewer(tab, true)
        : tab.model && !this.isDirty(tab)
          ? () => void this.reload(tab)
          : null;
      // A diff tab's key is diff:<rev>:<file>.
      this.onTabMenu(tab.path.replace(/^diff:[^:]*:/, ""), reload);
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
    const diff = tab?.diff ?? null;
    const preview = tab?.preview ?? null;
    this.host.style.visibility = hasText ? "visible" : "hidden";
    this.host.style.display = viewer || diff || preview ? "none" : "";
    this.diffHost.hidden = !diff;
    if (diff) {
      this.getDiffEditor().setModel(diff);
      this.position("");
    }
    this.viewerHost.hidden = !viewer && !preview;
    const shown = viewer?.el ?? preview;
    // Re-attaching a frame reloads it; leave one that is already showing.
    if (!shown) this.viewerHost.replaceChildren();
    else if (this.viewerHost.firstChild !== shown) this.viewerHost.replaceChildren(shown);
    this.placeholder.hidden = hasText || !!viewer || !!diff;
    this.placeholder.textContent = tab ? tab.note : "Open a file from the tree.";
    this.editor.setModel(tab?.model ?? null);
    if (tab?.model) {
      if (tab.view) this.editor.restoreViewState(tab.view);
      if (!preview) this.editor.focus();
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
