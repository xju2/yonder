// Go to File (Cmd+P): type part of a path, pick a match, open it in a tab.
// In a git repository the list comes from `git ls-files`, which skips
// ignored files and is quick even on network filesystems; elsewhere the
// folder is walked, up to a limit. The same dialog switches workspaces.

import { asError, gitFiles, listDir } from "./api";
import { joinPath } from "./tree";

/** A folder outside git is walked until this many files are found. */
const WALK_LIMIT = 20000;
/** Folders listed at once during a walk. */
const WALK_PARALLEL = 8;
const SHOWN = 50;

const dialog = document.getElementById("finder") as HTMLDialogElement;
const input = document.getElementById("finder-input") as HTMLInputElement;
const list = document.getElementById("finder-list")!;

async function walk(root: string): Promise<string[]> {
  const files: string[] = [];
  let level = [""];
  while (level.length && files.length < WALK_LIMIT) {
    const next: string[] = [];
    for (let i = 0; i < level.length && files.length < WALK_LIMIT; i += WALK_PARALLEL) {
      await Promise.all(
        level.slice(i, i + WALK_PARALLEL).map(async (rel) => {
          const entries = await listDir(rel ? joinPath(root, rel) : root).catch(() => []);
          for (const e of entries) {
            // Hidden folders (.git, .cache, …) are rarely what is wanted.
            if (e.name.startsWith(".") && e.kind === "dir") continue;
            const p = rel ? `${rel}/${e.name}` : e.name;
            if (e.kind === "dir" && !e.symlink) next.push(p);
            else if (e.kind === "file") files.push(p);
          }
        }),
      );
    }
    level = next;
  }
  return files;
}

/**
 * How well `query` matches `path`, or -1 if its letters do not all appear in
 * order. Runs of letters, and matches in the file name, score higher.
 */
export function score(query: string, path: string): number {
  const p = path.toLowerCase();
  const nameStart = p.lastIndexOf("/") + 1;
  let s = 0;
  let run = 0;
  let at = 0;
  for (const ch of query.toLowerCase()) {
    const i = p.indexOf(ch, at);
    if (i < 0) return -1;
    run = i === at ? run + 1 : 1;
    s += run + (i >= nameStart ? 2 : 0) + (i === nameStart || p[i - 1] === "/" ? 3 : 0);
    at = i + 1;
  }
  // Shorter paths first among equals.
  return s - p.length / 1000;
}

/**
 * The quick-pick dialog: type part of an item, choose a match. Go to File
 * and Switch Workspace both use it.
 */
export class Picker {
  private items: string[] = [];
  private loading = false;
  private matches: string[] = [];
  private selected = 0;
  private onPick: (item: string) => void = () => {};
  private run = 0;

  constructor() {
    input.addEventListener("input", () => this.filter());
    input.addEventListener("keydown", (e) => {
      const step = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
      if (step) {
        e.preventDefault();
        this.select(this.selected + step);
      } else if (e.key === "Enter") {
        e.preventDefault();
        this.pick(this.selected);
      }
    });
  }

  /** Offer `items`; `fresh`, if given, replaces them when it arrives. */
  async open(
    label: string,
    items: string[],
    onPick: (item: string) => void,
    fresh?: Promise<string[]>,
  ) {
    if (dialog.open) return;
    const run = ++this.run;
    this.items = items;
    this.onPick = onPick;
    this.loading = !!fresh;
    dialog.setAttribute("aria-label", label);
    input.placeholder = `${label}…`;
    input.value = "";
    this.filter();
    dialog.showModal();
    if (!fresh) return;
    try {
      const got = await fresh;
      if (run !== this.run) return;
      this.items = got;
    } catch (e) {
      if (run === this.run && !this.items.length) this.note(asError(e).message);
      return;
    } finally {
      if (run === this.run) this.loading = false;
    }
    if (dialog.open) this.filter();
  }

  private filter() {
    const q = input.value.trim();
    this.matches = q
      ? this.items
          .map((f) => [score(q, f), f] as const)
          .filter(([s]) => s >= 0)
          .sort((a, b) => b[0] - a[0])
          .slice(0, SHOWN)
          .map(([, f]) => f)
      : this.items.slice(0, SHOWN);
    list.replaceChildren(
      ...this.matches.map((f, i) => {
        const li = document.createElement("li");
        li.setAttribute("role", "option");
        const slash = f.lastIndexOf("/");
        const name = document.createElement("span");
        name.textContent = f.slice(slash + 1);
        const dir = document.createElement("span");
        dir.className = "dir";
        dir.textContent = f.slice(0, Math.max(slash, 0));
        li.append(name, dir);
        li.addEventListener("click", () => this.pick(i));
        return li;
      }),
    );
    if (!this.matches.length) this.note(this.loading && !this.items.length ? "Listing…" : "No matches.");
    this.select(0);
  }

  private note(text: string) {
    const li = document.createElement("li");
    li.className = "note";
    li.textContent = text;
    list.replaceChildren(li);
  }

  private select(i: number) {
    if (!this.matches.length) return;
    this.selected = (i + this.matches.length) % this.matches.length;
    list.querySelectorAll("[role=option]").forEach((li, j) => {
      li.setAttribute("aria-selected", String(j === this.selected));
      if (j === this.selected) li.scrollIntoView({ block: "nearest" });
    });
  }

  private pick(i: number) {
    const f = this.matches[i];
    if (!f) return;
    dialog.close();
    this.onPick(f);
  }
}

export class Finder {
  private files: string[] = [];
  private filesRoot = "";

  constructor(
    private picker: Picker,
    private root: () => string | null,
    private onOpen: (path: string) => void,
  ) {}

  open() {
    const root = this.root();
    if (!root) return;
    if (root !== this.filesRoot) this.files = [];
    // Re-list each time: files come and go. The old list shows meanwhile.
    const fresh = gitFiles(root)
      .then((files) => files ?? walk(root))
      .then((files) => {
        if (this.root() === root) {
          this.files = files;
          this.filesRoot = root;
        }
        return files;
      });
    void this.picker.open("Go to file", this.files, (f) => this.onOpen(joinPath(root, f)), fresh);
  }
}
