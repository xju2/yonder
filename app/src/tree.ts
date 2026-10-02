// The file tree. Folders load lazily when expanded; "refresh" re-lists only
// the folders that are open, never the whole tree (cheap on Lustre and NFS).

import { asError, listDir, type Entry } from "./api";

/** Directories this large are truncated; the terminal is better for them. */
const MAX_SHOWN = 5000;

/** Folders re-listed at once during a refresh. */
const REFRESH_PARALLEL = 4;

interface Node {
  path: string;
  name: string;
  kind: Entry["kind"];
  symlink: boolean;
  expanded: boolean;
  children: Node[] | null;
  error: string | null;
  hiddenCount: number;
  li: HTMLLIElement;
}

export const joinPath = (dir: string, name: string) =>
  dir.endsWith("/") ? dir + name : `${dir}/${name}`;

export class FileTree {
  private root: Node | null = null;
  private active: string | null = null;
  private refreshing: Promise<void> | null = null;

  constructor(
    private container: HTMLElement,
    private onOpen: (path: string) => void,
  ) {}

  async setRoot(path: string) {
    const name = path === "/" ? "/" : path.split("/").pop()!;
    this.root = this.makeNode({ name, kind: "dir", symlink: false, size: 0 }, path);
    this.root.expanded = true;
    const ul = document.createElement("ul");
    ul.className = "tree-root";
    ul.setAttribute("role", "tree");
    ul.append(this.root.li);
    this.container.replaceChildren(ul);
    await this.load(this.root);
  }

  /** Re-list every expanded folder, keeping what is expanded. */
  refresh(): Promise<void> {
    // Repeated focus events while a refresh runs share it.
    this.refreshing ??= this.refreshOpen().finally(() => (this.refreshing = null));
    return this.refreshing;
  }

  private async refreshOpen() {
    const open: Node[] = [];
    const walk = (n: Node) => {
      if (n.kind === "dir" && n.expanded) {
        open.push(n);
        n.children?.forEach(walk);
      }
    };
    if (this.root) walk(this.root);
    // A few at a time: a large expanded tree must not flood the connection.
    let next = 0;
    const worker = async () => {
      while (next < open.length) await this.load(open[next++]);
    };
    await Promise.all(Array.from({ length: REFRESH_PARALLEL }, worker));
  }

  setActive(path: string | null) {
    this.active = path;
    this.container.querySelectorAll(".row.active").forEach((r) => r.classList.remove("active"));
    if (path) this.container.querySelector(`.row[data-path="${CSS.escape(path)}"]`)?.classList.add("active");
  }

  private makeNode(e: Entry, path: string): Node {
    const li = document.createElement("li");
    const node: Node = {
      path,
      name: e.name,
      kind: e.kind,
      symlink: e.symlink,
      expanded: false,
      children: null,
      error: null,
      hiddenCount: 0,
      li,
    };
    const row = document.createElement("div");
    row.className = `row ${e.kind}`;
    if (e.name.startsWith(".")) row.classList.add("dotfile");
    if (path === this.active) row.classList.add("active");
    row.dataset.path = path;
    row.title = e.symlink ? `${path} (symbolic link)` : path;
    row.setAttribute("role", "treeitem");
    row.tabIndex = 0;
    const twisty = document.createElement("span");
    twisty.className = "twisty";
    const label = document.createElement("span");
    label.className = "label";
    label.textContent = e.name + (e.symlink ? " ↗" : "");
    row.append(twisty, label);
    row.addEventListener("click", () => this.activate(node));
    row.addEventListener("keydown", (ev) => this.onKey(ev, node));
    li.append(row);
    this.paintTwisty(node);
    return node;
  }

  private paintTwisty(n: Node) {
    const t = n.li.querySelector(".twisty")!;
    t.textContent = n.kind === "dir" ? (n.expanded ? "▾" : "▸") : "";
    if (n.kind === "dir") n.li.firstElementChild!.setAttribute("aria-expanded", String(n.expanded));
  }

  /** Enter or Space opens; arrows move between visible rows and fold folders. */
  private onKey(ev: KeyboardEvent, n: Node) {
    const rows = [...this.container.querySelectorAll<HTMLElement>(".row")].filter(
      (r) => r.offsetParent !== null,
    );
    const i = rows.indexOf(ev.currentTarget as HTMLElement);
    switch (ev.key) {
      case "Enter":
      case " ":
        void this.activate(n);
        break;
      case "ArrowDown":
        rows[i + 1]?.focus();
        break;
      case "ArrowUp":
        rows[i - 1]?.focus();
        break;
      case "ArrowRight":
        if (n.kind === "dir" && !n.expanded) void this.activate(n);
        break;
      case "ArrowLeft":
        if (n.kind === "dir" && n.expanded) void this.activate(n);
        break;
      default:
        return;
    }
    ev.preventDefault();
  }

  private async activate(n: Node) {
    if (n.kind === "dir") {
      n.expanded = !n.expanded;
      this.paintTwisty(n);
      if (n.expanded) await this.load(n);
      else n.li.querySelector(":scope > ul")?.remove();
    } else if (n.kind === "file") {
      this.onOpen(n.path);
    }
  }

  private async load(n: Node) {
    try {
      const entries = await listDir(n.path);
      entries.sort(
        (a, b) =>
          Number(b.kind === "dir") - Number(a.kind === "dir") ||
          a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" }),
      );
      // Keep the expansion state of folders that are still there.
      const before = new Map((n.children ?? []).map((c) => [c.name, c]));
      n.hiddenCount = Math.max(0, entries.length - MAX_SHOWN);
      n.children = entries.slice(0, MAX_SHOWN).map((e) => {
        const old = before.get(e.name);
        if (old && old.kind === e.kind) return old;
        return this.makeNode(e, joinPath(n.path, e.name));
      });
      n.error = null;
    } catch (e) {
      n.children = [];
      n.error = asError(e).message;
    }
    if (n.expanded) this.paintChildren(n);
  }

  private paintChildren(n: Node) {
    const ul = document.createElement("ul");
    ul.setAttribute("role", "group");
    for (const c of n.children ?? []) ul.append(c.li);
    const note = (text: string) => {
      const li = document.createElement("li");
      li.className = "note";
      li.textContent = text;
      ul.append(li);
    };
    if (n.error) note(n.error);
    else if (n.children?.length === 0) note("empty");
    if (n.hiddenCount) note(`… ${n.hiddenCount} more not shown`);
    n.li.querySelector(":scope > ul")?.remove();
    n.li.append(ul);
  }
}
