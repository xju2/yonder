import { Menu } from "@tauri-apps/api/menu";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "./api";
import { baseName, Editors } from "./editor";
import { Finder, Picker } from "./finder";
import { GitPanel } from "./git";
import { Jupyter } from "./jupyter";
import { Languages } from "./lsp";
import { ask, askText, tell } from "./modal";
import { SearchPanel } from "./search";
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
/** The open workspace as typed: what reconnecting connects to. */
let target = { host: "", folder: "" };
/** The open workspace as resolved: host and folder. */
let wsKey: string | null = null;
/** Live connections by host; the workspaces on a host share one. */
const hosts = new Map<string, api.ConnInfo>();

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

/** Offer this Mac and the hosts in ~/.ssh/config; typing any destination still works. */
async function paintHosts() {
  const hosts = await api.sshHosts().catch(() => ["local"]);
  $("host-list").replaceChildren(
    ...hosts.map((h) => {
      const o = document.createElement("option");
      o.value = h;
      if (h === "local") o.label = "Local folder on this Mac";
      return o;
    }),
  );
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

// ssh's questions: passwords and one-time codes, or whether to trust a host.
void api.onAskpass(async ({ id, prompt }) => {
  const text = prompt.trim();
  let answer: string | null;
  if (/\(yes\/no/i.test(text)) {
    const choice = await ask(
      "Trust this host?",
      [
        { value: "no", label: "Cancel" },
        { value: "yes", label: "Trust and connect", primary: true },
      ],
      text,
    );
    answer = choice === "yes" ? "yes" : null;
  } else {
    answer = await askText(text || "Password:", true, target.host ? `ssh ${target.host}` : undefined);
  }
  await api.askpassAnswer(id, answer);
});

/** The connection dropped while another attempt ran; that attempt decides. */
let droppedWhileConnecting: string | null = null;

void api.onClosed((e) => {
  for (const [h, c] of hosts) if (c.generation === e.generation) hosts.delete(h);
  terminals.disconnected(e.generation);
  if (!conn || e.generation !== conn.generation) return;
  if (connecting) droppedWhileConnecting = e.reason;
  else connectionLost(e.reason);
});

function connectionLost(reason: string) {
  statusConn.classList.add("down");
  scheduleReconnect(reason);
}

// ---- reconnecting on its own

/** Seconds between attempts; the last repeats. */
const RETRY_DELAYS = [1, 2, 5, 10, 20, 30];
const retry = { attempt: 0, timer: 0, tick: 0 };

/** One connection attempt at a time: a second would share its password
 * prompts, and whichever finished last would win. */
let connecting = false;

function stopRetrying() {
  clearTimeout(retry.timer);
  clearInterval(retry.tick);
  retry.timer = retry.tick = 0;
}

function showBanner(text: string, button: string | null) {
  banner.replaceChildren();
  const span = document.createElement("span");
  span.textContent = text;
  banner.append(span);
  if (button) {
    const btn = document.createElement("button");
    btn.textContent = button;
    btn.addEventListener("click", () => void reconnectNow());
    banner.append(btn);
  }
  banner.hidden = false;
}

function scheduleReconnect(reason: string) {
  stopRetrying();
  const host = conn?.host ?? target.host;
  reason = reason.replace(/[.\s]+$/, "");
  let left = RETRY_DELAYS[Math.min(retry.attempt, RETRY_DELAYS.length - 1)];
  const paint = () =>
    showBanner(
      `Disconnected from ${host}: ${reason}. Reconnecting in ${left} s… Your open files and edits are kept.`,
      "Reconnect now",
    );
  paint();
  retry.tick = window.setInterval(() => {
    left = Math.max(0, left - 1);
    paint();
  }, 1000);
  retry.timer = window.setTimeout(() => void reconnectNow(), left * 1000);
}

/** Failures that retrying cannot fix: the person has to act. */
function needsPerson(e: api.CmdError) {
  return /permission denied|host key|authentication/i.test(`${e.message} ${e.hint ?? ""}`);
}

async function reconnectNow() {
  if (connecting) return;
  stopRetrying();
  connecting = true;
  showBanner(`Reconnecting to ${target.host}…`, null);
  try {
    const oldRoot = conn?.root;
    const info = await api.connect(target.host, target.folder || "~");
    hosts.set(info.host, info);
    retry.attempt = 0;
    await startSession(info);
    // The folder may resolve differently now (a moved symlink, say).
    if (info.root === oldRoot) await tree.refresh();
    else await tree.setRoot(info.root);
    wsKey = `${info.host}:${info.root}`;
    void git.refreshChanges();
    status(`Reconnected to ${info.host}`);
  } catch (err) {
    const ce = api.asError(err);
    if (needsPerson(ce)) {
      showBanner(`Could not reconnect: ${ce.message}`, "Try again");
    } else {
      retry.attempt++;
      scheduleReconnect(ce.message);
    }
  } finally {
    connecting = false;
    droppedWhileConnecting = null;
  }
}

// Back online after sleep or a network change: try at once.
window.addEventListener("online", () => {
  if (conn && statusConn.classList.contains("down")) void reconnectNow();
});

// ---- views

async function copyPath(path: string | null, relative: boolean) {
  if (!path || !conn) return;
  const root = conn.root.replace(/\/$/, "");
  let text = path;
  if (relative && path === root) text = ".";
  else if (relative && path.startsWith(root + "/")) text = path.slice(root.length + 1);
  try {
    await api.copyText(text);
    status(`Copied ${text}`);
  } catch (e) {
    status(`Could not copy: ${api.asError(e).message}`);
  }
}

let compareBase: string | null = null;

const pathMenu = (path: string, reload: (() => void) | null = null) =>
  void Menu.new({
    items: [
      {
        text: "Select for Comparison",
        action: () => {
          compareBase = path;
          status(`Selected ${baseName(path)} for comparison.`);
        },
      },
      {
        text: compareBase ? `Compare with Selected (${baseName(compareBase)})` : "Compare with Selected",
        enabled: !!compareBase && compareBase !== path,
        action: () => {
          if (compareBase) void editors.compare(compareBase, path);
        },
      },
      ...(reload ? [{ text: "Reload", action: reload }] : []),
      ...(/\.ipynb$/i.test(path) && conn ? [{ text: "Show Jupyter Log", action: showJupyterLog }] : []),
      { text: "Copy Path", action: () => void copyPath(path, false) },
      { text: "Copy Relative Path", action: () => void copyPath(path, true) },
    ],
  }).then((m) => m.popup());

function showJupyterLog() {
  const log = conn && jupyter.log(conn.host, conn.root);
  const lines = (log ?? "").trim().split("\n").slice(-60).join("\n");
  void tell("Jupyter log", lines || "Jupyter has not been started in this workspace.");
}

const refreshTree = () => {
  void tree.refresh();
  void git.refreshChanges();
};
const tree = new FileTree($("tree"), (path) => void editors.open(path), (path) =>
  pathMenu(path, refreshTree),
);

/** Show `path` in the file tree: double-clicking a tab does this. */
async function reveal(path: string) {
  workspace.classList.remove("sidebar-hidden");
  await showSideView("files");
  if (!(await tree.reveal(path))) status(`${baseName(path)} is not in the folder's tree.`);
}

const editors = new Editors(
  $("editor"),
  $("viewer"),
  $("diff"),
  $("tabs"),
  $("placeholder"),
  (msg) => status(msg),
  (path) => {
    tree.setActive(path);
    // Runs whenever a tab opens, closes or comes to the front.
    saveTabs();
  },
  (pos) => ($("status-pos").textContent = pos),
  () => void git.refreshChanges(),
  pathMenu,
  (path) => void reveal(path),
  (path) => jupyter.page(conn!.host, conn!.root, path),
);
const jupyter = new Jupyter((host) => hosts.get(host)?.generation ?? null);
new Languages(
  () => (conn ? { host: conn.host, root: conn.root } : null),
  (host) => hosts.get(host)?.generation ?? null,
  (path, at) => void editors.open(path, at),
  (msg) => status(msg),
);
const picker = new Picker();
const finder = new Finder(
  picker,
  () => (conn && !workspace.hidden ? conn.root : null),
  (path) => void editors.open(path),
);
const search = new SearchPanel(
  $("search-view"),
  () => conn?.root ?? null,
  (path, at) => void editors.open(path, at),
);

const toggleSidebar = () => workspace.classList.toggle("sidebar-hidden");
$("toggle-sidebar").addEventListener("click", toggleSidebar);

// Menu items, so their shortcuts work wherever the keyboard is.
void api.onMenu((id) => {
  if (document.querySelector("dialog[open]")) return;
  if (id === "install-cli") return void installCli();
  if (!conn) return;
  if (id === "new-workspace") return showConnect();
  if (workspace.hidden) return;
  if (id === "go-to-file") finder.open();
  else if (id === "switch-workspace") pickWorkspace();
  else if (id === "find-in-folder") {
    workspace.classList.remove("sidebar-hidden");
    void showSideView("search");
    search.focus(editors.selection());
  }
  else if (id === "toggle-sidebar") toggleSidebar();
  else if (id === "markdown-preview") editors.togglePreview();
  else if (id === "split-editor") editors.toggleSplit();
  else if (id === "copy-path") void copyPath(tree.chosen(), false);
  else if (id === "copy-relative-path") void copyPath(tree.chosen(), true);
  // Cmd+W closes the terminal that has the keyboard, else the editor tab.
  else if (id === "close-tab" && !terminals.closeFocused()) editors.closeActive();
});
const terminals = new TerminalPanel(
  $("terminal-panel"),
  $("panel-sash"),
  $("term-tabs"),
  $("term-body"),
  () => conn?.root ?? null,
  () => conn?.generation ?? null,
  (path) => void editors.open(path),
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
  api.useConnection(info.generation);
  stopRetrying();
  retry.attempt = 0;
  git.reset();
  if (sideView !== "files") void showSideView(sideView);
  connectView.hidden = true;
  workspace.hidden = false;
  banner.hidden = true;
  statusConn.classList.remove("down");
  statusConn.textContent = `${info.host}:${info.root}`;
  statusConn.title = `Connected to ${info.hostname}. Click to switch workspace (⌥⌘O).`;
  const home = info.home.replace(/\/$/, "");
  const shown =
    info.root === home ? "~" : info.root.startsWith(home + "/") ? "~" + info.root.slice(home.length) : info.root;
  $("root-name").textContent = shown;
  $("root-name").title = info.root;
  const title = `${info.root.split("/").pop() || "/"} — ${info.host} — Yonder`;
  document.title = title;
  void getCurrentWindow().setTitle(title);
}

// ---- workspaces: one folder on one host each, switched in this window

/**
 * Open `folder` on `host`, putting the current workspace aside: its tabs,
 * unsaved edits, terminals and tree come back as they were when it is opened
 * again. Each host's connection stays open, so going back needs no login.
 */
async function openWorkspace(host: string, folder: string): Promise<boolean> {
  if (connecting) return false;
  connecting = true;
  let ok = false;
  let reopen: SavedTabs | null = null;
  try {
    const live = hosts.get(host);
    const info: api.ConnInfo = live
      ? { ...live, root: await api.openFolder(live.generation, folder || "~") }
      : await api.connect(host, folder || "~");
    hosts.set(host, info);
    ok = true;
    target = { host, folder };
    saveRecent(target);
    await startSession(info);
    const key = `${info.host}:${info.root}`;
    const from = wsKey;
    if (key !== from) {
      wsKey = key;
      // Read first: switching saves the (still empty) tabs of `key`.
      const saved = loadTabs()[key] ?? null;
      if (!editors.switchTo(from, key)) reopen = saved;
      terminals.switchTo(key);
      search.reset();
      await tree.switchTo(from, key, info.root);
    }
    void git.refreshChanges();
  } catch (err) {
    const ce = api.asError(err);
    if (connectView.hidden) {
      void tell(`Could not open ${host}:${folder || "~"}.`, [ce.message, ce.hint].filter(Boolean).join("\n\n"));
    } else {
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
    }
  } finally {
    connecting = false;
    const dropped = droppedWhileConnecting;
    droppedWhileConnecting = null;
    if (!ok && dropped) connectionLost(dropped);
  }
  if (reopen) {
    restoring = true;
    await editors.reopen(reopen.files, reopen.active).finally(() => (restoring = false));
    saveTabs();
  }
  return ok;
}

// ---- each workspace's open files, kept for the next session

interface SavedTabs {
  files: string[];
  active: string | null;
}
const TABS_KEY = "yonder.tabs";
/** Set while saved tabs reopen, so the half-open list is not saved over them. */
let restoring = false;

function loadTabs(): Record<string, SavedTabs> {
  try {
    return JSON.parse(localStorage.getItem(TABS_KEY) ?? "{}");
  } catch {
    return {};
  }
}

function saveTabs() {
  if (!wsKey || restoring) return;
  try {
    localStorage.setItem(TABS_KEY, JSON.stringify({ ...loadTabs(), [wsKey]: editors.openFiles() }));
  } catch {
    /* storage unavailable */
  }
}

/** Switch Workspace (⌥⌘O): pick a recent folder. */
function pickWorkspace() {
  const label = (r: Recent) => `${r.host}:${r.folder || "~"}`;
  const others = new Map(
    loadRecent()
      .filter((r) => r.host !== target.host || r.folder !== target.folder)
      .map((r) => [label(r), r]),
  );
  void picker.open("Switch workspace", [...others.keys()], (item) => {
    const r = others.get(item)!;
    status(`Opening ${item}…`);
    void openWorkspace(r.host, r.folder);
  });
}

/** New Workspace (⇧⌘N): the connect screen, with a way back. */
function showConnect() {
  connectView.hidden = false;
  workspace.hidden = true;
  connectError.hidden = true;
  connectLog.replaceChildren();
  $("connect-cancel").hidden = !conn;
  hostInput.value = conn?.host ?? "";
  folderInput.value = "";
  paintRecent();
  void paintHosts();
  (conn ? folderInput : hostInput).focus();
}

function hideConnect() {
  if (!conn || connecting) return;
  connectView.hidden = true;
  workspace.hidden = false;
}

$("connect-cancel").addEventListener("click", hideConnect);
connectView.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideConnect();
});
statusConn.addEventListener("click", pickWorkspace);

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (connecting) return;
  connectLog.replaceChildren();
  connectError.hidden = true;
  connectBtn.disabled = true;
  connectBtn.textContent = "Opening…";
  await openWorkspace(hostInput.value.trim(), folderInput.value.trim());
  connectBtn.disabled = false;
  connectBtn.textContent = "Open";
});

$("refresh-tree").addEventListener("click", refreshTree);

// Refreshing on focus catches files written by batch jobs or other machines,
// which file-change notifications miss on network filesystems.
window.addEventListener("focus", () => {
  if (!conn || workspace.hidden) return;
  void tree.refresh();
  editors.refreshActive();
  // Also recolours the tree, so it runs whichever view is showing.
  void git.refreshChanges();
});

// ---- sidebar views: Files, Changes, History

type SideView = "files" | "changes" | "history" | "search";
let sideView: SideView = "files";
const git = new GitPanel(
  $("changes-view"),
  $("history-view"),
  () => conn?.root ?? null,
  (req) => void editors.openDiff(req),
  (repo, status) => tree.setGit(repo, status),
);

async function showSideView(view: SideView) {
  sideView = view;
  for (const b of document.querySelectorAll<HTMLButtonElement>("#side-tabs button")) {
    b.setAttribute("aria-selected", String(b.dataset.view === view));
    // Only the selected tab is in the Tab order; arrows move between tabs.
    b.tabIndex = b.dataset.view === view ? 0 : -1;
  }
  $("files-view").hidden = view !== "files";
  $("changes-view").hidden = view !== "changes";
  $("history-view").hidden = view !== "history";
  $("search-view").hidden = view !== "search";
  // Changes are re-read each time: they move with every save.
  if (view === "changes") await git.refreshChanges();
  if (view === "history") await git.showHistory();
}

const sideTabs = [...document.querySelectorAll<HTMLButtonElement>("#side-tabs button")];
for (const [i, b] of sideTabs.entries()) {
  b.addEventListener("click", () => void showSideView(b.dataset.view as SideView));
  b.addEventListener("keydown", (e) => {
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const next = sideTabs[(i + step + sideTabs.length) % sideTabs.length];
    next.focus();
    void showSideView(next.dataset.view as SideView);
  });
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

// ---- quitting or closing with unsaved edits

let askingToQuit = false;
async function confirmQuit() {
  // Cmd+Q pressed twice should not stack two questions.
  if (askingToQuit) return;
  askingToQuit = true;
  const unsaved = editors.hasUnsaved();
  const choice = await ask(
    unsaved ? "Some files have unsaved changes." : "Quit Yonder?",
    [
      { value: "cancel", label: unsaved ? "Keep editing" : "Cancel", primary: true },
      { value: "quit", label: unsaved ? "Quit without saving" : "Quit", danger: unsaved },
    ],
    conn ? "Terminals on the remote will be closed." : undefined,
  );
  askingToQuit = false;
  if (choice === "quit") await api.quitApp();
}

// Every quit (Cmd+Q, the Dock) is held until this answers: a stray Cmd+Q
// would otherwise drop the connection and every terminal.
void api.onQuitRequested(() => void confirmQuit());

void getCurrentWindow().onCloseRequested(async (event) => {
  if (!editors.hasUnsaved()) return;
  event.preventDefault();
  await confirmQuit();
});

async function installCli() {
  try {
    const at = await api.installCli();
    await tell("Installed the yonder command.", `${at} is linked. In a terminal, \`yonder .\` opens the current folder.`);
  } catch (err) {
    const msg = String(err);
    // Cancelling the password prompt is not an error worth showing.
    if (!/User canceled|-128/.test(msg)) await tell("Could not install the yonder command.", msg);
  }
}

// `yonder .` in a terminal: open that folder on this Mac. Listen first, so
// none sent while the window loads is missed.
async function openHanded() {
  for (const o of await api.takeOpened()) {
    if (o.is_dir) await openWorkspace("local", o.path);
    else await showFile("local", o.path);
  }
}

/** Open a file on `host`, switching to a workspace there if this one is elsewhere. */
async function showFile(host: string, path: string) {
  if (conn?.host !== host && !(await openWorkspace(host, path.replace(/\/[^/]*$/, "") || "/"))) return;
  await editors.open(path);
}
// `yonder FILE` in an OS terminal, here or over ssh.
void api.onOpenFile((e) => {
  for (const [host, c] of hosts) if (c.generation === e.generation) void showFile(host, e.path);
});
void api.onOpened(() => void openHanded()).then(openHanded);

paintRecent();
void paintHosts();
