import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "./api";
import { Editors } from "./editor";
import { GitPanel } from "./git";
import { ask } from "./modal";
import { TerminalPanel } from "./terminal";
import { FileTree } from "./tree";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const connectView = $("connect-view");
const workspace = $("workspace");
const form = $<HTMLFormElement>("connect-form");
const hostInput = $<HTMLInputElement>("host");
const folderInput = $<HTMLInputElement>("folder");
const connectBtn = $<HTMLButtonElement>("connect-btn");
const connectLog = $("connect-log");
const connectError = $("connect-error");
const banner = $("banner");
const statusConn = $("status-conn");
const statusMsg = $("status-msg");

let conn: api.ConnInfo | null = null;
let target = { host: "", folder: "" };

// ---- recent connections (a per-machine convenience; failures are harmless)

interface Recent {
  host: string;
  folder: string;
}
const RECENT_KEY = "yonder.recent";

function loadRecent(): Recent[] {
  try {
    return JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]");
  } catch {
    return [];
  }
}

function saveRecent(r: Recent) {
  const list = [r, ...loadRecent().filter((x) => x.host !== r.host || x.folder !== r.folder)];
  try {
    localStorage.setItem(RECENT_KEY, JSON.stringify(list.slice(0, 8)));
  } catch {
    /* storage unavailable */
  }
}

function paintRecent() {
  const box = $("recent");
  const list = loadRecent();
  box.replaceChildren(
    ...list.map((r) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "recent";
      b.textContent = `${r.host}:${r.folder || "~"}`;
      b.addEventListener("click", () => {
        hostInput.value = r.host;
        folderInput.value = r.folder;
        form.requestSubmit();
      });
      return b;
    }),
  );
  if (list.length === 0) hostInput.focus();
}

// ---- connection log and status

let statusTimer = 0;
const status = (msg: string) => {
  statusMsg.textContent = msg;
  clearTimeout(statusTimer);
  if (msg) statusTimer = window.setTimeout(() => (statusMsg.textContent = ""), 6000);
};

void api.onLog((e) => {
  const li = document.createElement("li");
  li.className = e.level;
  li.textContent = e.message;
  connectLog.append(li);
  li.scrollIntoView({ block: "nearest" });
  if (!connectView.hidden) return;
  if (e.level === "step") status(e.message);
});

void api.onClosed((e) => {
  if (!conn || e.generation !== conn.generation) return;
  statusConn.classList.add("down");
  terminals.disconnected();
  showBanner(`Disconnected from ${conn.host}: ${e.reason}. Your open files and edits are kept.`);
});

function showBanner(text: string) {
  banner.replaceChildren();
  const span = document.createElement("span");
  span.textContent = text;
  const btn = document.createElement("button");
  btn.textContent = "Reconnect";
  btn.addEventListener("click", () => void reconnect(btn));
  banner.append(span, btn);
  banner.hidden = false;
}

// ---- views

const tree = new FileTree($("tree"), (path) => void editors.open(path));
const editors = new Editors(
  $("editor"),
  $("viewer"),
  $("diff"),
  $("tabs"),
  $("placeholder"),
  (msg) => status(msg),
  (path) => tree.setActive(path),
  (pos) => ($("status-pos").textContent = pos),
);
const terminals = new TerminalPanel(
  $("terminal-panel"),
  $("panel-sash"),
  $("term-tabs"),
  $("term-body"),
  () => conn?.root ?? null,
);

$("toggle-terminal").addEventListener("click", () => terminals.toggle());
$("term-new").addEventListener("click", () => void terminals.create());
$("term-hide").addEventListener("click", () => terminals.hide());
// Ctrl+` shows and hides terminals, as in most editors. Caught before the
// editor or a terminal sees it.
window.addEventListener(
  "keydown",
  (e) => {
    if (e.ctrlKey && e.key === "`" && conn && !workspace.hidden) {
      e.preventDefault();
      e.stopPropagation();
      terminals.toggle();
    }
  },
  true,
);

async function startSession(info: api.ConnInfo) {
  conn = info;
  git.reset();
  if (sideView !== "files") void showSideView(sideView);
  connectView.hidden = true;
  workspace.hidden = false;
  banner.hidden = true;
  statusConn.classList.remove("down");
  statusConn.textContent = `${info.host}:${info.root}`;
  statusConn.title = `Connected to ${info.hostname}`;
  const home = info.home.replace(/\/$/, "");
  const shown =
    info.root === home ? "~" : info.root.startsWith(home + "/") ? "~" + info.root.slice(home.length) : info.root;
  $("root-name").textContent = shown;
  $("root-name").title = info.root;
  const title = `${info.root.split("/").pop() || "/"} — ${info.host} — Yonder`;
  document.title = title;
  void getCurrentWindow().setTitle(title);
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  target = { host: hostInput.value.trim(), folder: folderInput.value.trim() };
  connectLog.replaceChildren();
  connectError.hidden = true;
  connectBtn.disabled = true;
  connectBtn.textContent = "Connecting…";
  try {
    const info = await api.connect(target.host, target.folder || "~");
    saveRecent(target);
    await startSession(info);
    await tree.setRoot(info.root);
  } catch (err) {
    const ce = api.asError(err);
    connectError.replaceChildren();
    const msg = document.createElement("p");
    msg.textContent = ce.message;
    connectError.append(msg);
    if (ce.hint) {
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = ce.hint;
      connectError.append(hint);
    }
    connectError.hidden = false;
  } finally {
    connectBtn.disabled = false;
    connectBtn.textContent = "Connect";
  }
});

async function reconnect(btn: HTMLButtonElement) {
  btn.disabled = true;
  btn.textContent = "Reconnecting…";
  try {
    const oldRoot = conn?.root;
    const info = await api.connect(target.host, target.folder || "~");
    await startSession(info);
    // The folder may resolve differently now (a moved symlink, say).
    if (info.root === oldRoot) await tree.refresh();
    else await tree.setRoot(info.root);
    status(`Reconnected to ${info.host}`);
  } catch (err) {
    const ce = api.asError(err);
    btn.disabled = false;
    btn.textContent = "Reconnect";
    await ask(
      "Could not reconnect.",
      [{ value: "ok", label: "OK", primary: true }],
      [ce.message, ce.hint].filter(Boolean).join("\n\n"),
    );
  }
}

$("refresh-tree").addEventListener("click", () => void tree.refresh());

// Refreshing on focus catches files written by batch jobs or other machines,
// which file-change notifications miss on network filesystems.
window.addEventListener("focus", () => {
  if (!conn || workspace.hidden) return;
  void tree.refresh();
  editors.refreshActive();
  if (sideView === "changes") void git.refreshChanges();
});

// ---- sidebar views: Files, Changes, History

type SideView = "files" | "changes" | "history";
let sideView: SideView = "files";
const git = new GitPanel(
  $("changes-view"),
  $("history-view"),
  () => conn?.root ?? null,
  (req) => void editors.openDiff(req),
);

async function showSideView(view: SideView) {
  sideView = view;
  for (const b of document.querySelectorAll<HTMLButtonElement>("#side-tabs button")) {
    b.setAttribute("aria-selected", String(b.dataset.view === view));
  }
  $("files-view").hidden = view !== "files";
  $("changes-view").hidden = view !== "changes";
  $("history-view").hidden = view !== "history";
  // Changes are re-read each time: they move with every save.
  if (view === "changes") await git.refreshChanges();
  if (view === "history") await git.showHistory();
}

for (const b of document.querySelectorAll<HTMLButtonElement>("#side-tabs button")) {
  b.addEventListener("click", () => void showSideView(b.dataset.view as SideView));
}

// ---- terminal panel height

const panelSash = $("panel-sash");
const panelBounds = () => {
  const main = $("main").getBoundingClientRect();
  return { main, min: 80, max: main.height - 120 };
};
const setPanelHeight = (h: number) => {
  const { min, max } = panelBounds();
  const clamped = Math.round(Math.min(Math.max(h, min), max));
  document.documentElement.style.setProperty("--panel-height", `${clamped}px`);
  panelSash.setAttribute("aria-valuenow", String(clamped));
};
panelSash.tabIndex = 0;
panelSash.setAttribute("role", "separator");
panelSash.setAttribute("aria-orientation", "horizontal");
panelSash.setAttribute("aria-label", "Terminal panel height");
panelSash.addEventListener("pointerdown", (e) => {
  panelSash.setPointerCapture(e.pointerId);
  const { main } = panelBounds();
  const move = (ev: PointerEvent) => setPanelHeight(main.bottom - ev.clientY);
  panelSash.addEventListener("pointermove", move);
  panelSash.addEventListener("pointerup", () => panelSash.removeEventListener("pointermove", move), {
    once: true,
  });
});
// Arrow keys move the divider too, 20 px a press.
panelSash.addEventListener("keydown", (e) => {
  const step = e.key === "ArrowUp" ? 20 : e.key === "ArrowDown" ? -20 : 0;
  if (!step) return;
  e.preventDefault();
  setPanelHeight($("terminal-panel").getBoundingClientRect().height + step);
});

// ---- sidebar width

const sash = $("sash");
sash.addEventListener("pointerdown", (e) => {
  sash.setPointerCapture(e.pointerId);
  const move = (ev: PointerEvent) => {
    const w = Math.min(Math.max(ev.clientX, 140), window.innerWidth - 300);
    document.documentElement.style.setProperty("--sidebar", `${w}px`);
  };
  sash.addEventListener("pointermove", move);
  sash.addEventListener("pointerup", () => sash.removeEventListener("pointermove", move), {
    once: true,
  });
});

// ---- closing the window with unsaved edits

void getCurrentWindow().onCloseRequested(async (event) => {
  if (!editors.hasUnsaved()) return;
  event.preventDefault();
  const choice = await ask("Some files have unsaved changes.", [
    { value: "cancel", label: "Keep editing", primary: true },
    { value: "quit", label: "Close without saving", danger: true },
  ]);
  if (choice === "quit") {
    await api.disconnect();
    await getCurrentWindow().destroy();
  }
});

paintRecent();
