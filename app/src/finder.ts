// Go to File (Cmd+P): type part of a path, pick a match, open it in a tab.
// In a git repository the list comes from `git ls-files`, which skips
// ignored files and is quick even on network filesystems; elsewhere the
// folder is walked, up to a limit.

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

export class Finder {
  private files: string[] = [];
  private filesRoot = "";
  private matches: string[] = [];
  private selected = 0;

  constructor(
    private root: () => string | null,
    private onOpen: (path: string) => void,
  ) {
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

  async open() {
    const root = this.root();
    if (!root || dialog.open) return;
    if (root !== this.filesRoot) this.files = [];
    input.value = "";
    this.filter();
    dialog.showModal();
    // Re-list each time: files come and go. The old list shows meanwhile.
    try {
      const files = (await gitFiles(root)) ?? (await walk(root));
      if (this.root() !== root) return;
      this.files = files;
      this.filesRoot = root;
    } catch (e) {
      if (!this.files.length) this.note(asError(e).message);
      return;
    }
    if (dialog.open) this.filter();
  }

  private filter() {
    const q = input.value.trim();
    this.matches = q
      ? this.files
          .map((f) => [score(q, f), f] as const)
          .filter(([s]) => s >= 0)
          .sort((a, b) => b[0] - a[0])
          .slice(0, SHOWN)
          .map(([, f]) => f)
      : this.files.slice(0, SHOWN);
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
    if (!this.matches.length) this.note(this.files.length ? "No matching files." : "Listing files…");
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
    const root = this.root();
    if (!f || !root) return;
    dialog.close();
    this.onOpen(joinPath(root, f));
  }
}
