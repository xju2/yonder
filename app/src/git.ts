// The sidebar's Changes and History views. Read-only: they show what git
// knows and open diffs; committing stays in the terminal.

import * as api from "./api";

export interface DiffRequest {
  repo: string;
  path: string;
  oldPath: string | null;
  /** null: uncommitted changes. Otherwise the commit to show. */
  rev: string | null;
  /** Short hash, for the tab title. */
  short: string | null;
}

const PAGE = 100;

const STATUS_NAMES: Record<string, string> = {
  M: "modified",
  A: "added",
  D: "deleted",
  R: "renamed",
  C: "copied",
  U: "conflict",
  "?": "untracked",
};

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export function relativeTime(seconds: number, now = Date.now() / 1000): string {
  const d = Math.max(0, now - seconds);
  const units: [number, string][] = [
    [365 * 86400, "year"],
    [30 * 86400, "month"],
    [7 * 86400, "week"],
    [86400, "day"],
    [3600, "hour"],
    [60, "minute"],
  ];
  for (const [size, name] of units) {
    const n = Math.floor(d / size);
    if (n >= 1) return `${n} ${name}${n === 1 ? "" : "s"} ago`;
  }
  return "just now";
}

/** A row for one changed file: status letter, name, and folder. */
function changeRow(c: api.GitChange, onOpen: () => void): HTMLElement {
  const row = el("div", "git-file");
  row.tabIndex = 0;
  row.setAttribute("role", "button");
  const name = c.path.split("/").pop()!;
  const dir = c.path.slice(0, c.path.length - name.length).replace(/\/$/, "");
  const letter = el("span", `git-status s-${c.status === "?" ? "q" : c.status}`, c.status);
  letter.title = STATUS_NAMES[c.status] ?? c.status;
  row.title = c.old_path ? `${c.old_path} → ${c.path}` : c.path;
  row.append(letter, el("span", "git-name", name), el("span", "git-dir", dir));
  row.addEventListener("click", onOpen);
  row.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onOpen();
    }
  });
  return row;
}

export class GitPanel {
  private repo: string | null = null;
  private loadedSkip = 0;
  private historyDone = false;
  private changesBusy: Promise<void> | null = null;
  /** Bumped by reset(); results from older requests are dropped. */
  private session = 0;
  /** Bumped by each history reload; older pages are dropped. */
  private historyRun = 0;

  constructor(
    private changesEl: HTMLElement,
    private historyEl: HTMLElement,
    /** The open folder. */
    private dir: () => string | null,
    private openDiff: (req: DiffRequest) => void,
  ) {}

  /** Forget everything; called when the folder or connection changes. */
  reset() {
    this.session++;
    this.historyRun++;
    this.changesBusy = null;
    this.repo = null;
    this.loadedSkip = 0;
    this.historyDone = false;
    this.changesEl.replaceChildren();
    this.historyEl.replaceChildren();
  }

  /** Re-run `git status`. Overlapping calls share one run. */
  refreshChanges(): Promise<void> {
    if (!this.changesBusy) {
      const run: Promise<void> = this.loadChanges().finally(() => {
        if (this.changesBusy === run) this.changesBusy = null;
      });
      this.changesBusy = run;
    }
    return this.changesBusy;
  }

  private header(view: HTMLElement, title: string, onRefresh: () => void): HTMLElement {
    const h = el("header");
    h.append(el("span", "git-title", title));
    const b = el("button", undefined, "↻");
    b.title = "Refresh";
    b.setAttribute("aria-label", "Refresh");
    b.addEventListener("click", onRefresh);
    h.append(b);
    view.replaceChildren(h);
    return h;
  }

  private async loadChanges() {
    const dir = this.dir();
    if (!dir) return;
    const session = this.session;
    const body = el("div", "git-body");
    if (!this.changesEl.firstChild) {
      this.header(this.changesEl, "Changes", () => void this.refreshChanges());
      this.changesEl.append(el("p", "git-note", "Running git status…"));
    }
    let result;
    try {
      result = await api.gitStatus(dir);
    } catch (e) {
      if (session !== this.session) return;
      this.header(this.changesEl, "Changes", () => void this.refreshChanges());
      this.changesEl.append(el("p", "git-note error", api.asError(e).message));
      return;
    }
    // A reconnect or another folder made this answer stale.
    if (session !== this.session) return;
    const { repo, status } = result;
    this.repo = repo;
    const branch = !status
      ? "Changes"
      : status.branch
        ? status.branch +
          (status.ahead ? ` ↑${status.ahead}` : "") +
          (status.behind ? ` ↓${status.behind}` : "")
        : "detached HEAD";
    const h = this.header(this.changesEl, branch, () => void this.refreshChanges());
    if (status?.upstream) h.title = `Tracking ${status.upstream}`;
    if (!repo || !status) {
      body.append(el("p", "git-note", "This folder is not in a git repository."));
    } else if (status.files.length === 0) {
      body.append(el("p", "git-note", "No changes since the last commit."));
    } else {
      for (const c of status.files) {
        body.append(
          changeRow(c, () =>
            this.openDiff({ repo, path: c.path, oldPath: c.old_path, rev: null, short: null }),
          ),
        );
      }
    }
    this.changesEl.append(body);
  }

  /** Show the history, loading the first page if nothing is loaded yet. */
  async showHistory() {
    if (this.loadedSkip > 0 || this.historyDone) return;
    await this.reloadHistory();
  }

  private async reloadHistory() {
    const run = ++this.historyRun;
    this.loadedSkip = 0;
    this.historyDone = false;
    this.header(this.historyEl, "History", () => void this.reloadHistory());
    const list = el("div", "git-body");
    this.historyEl.append(list);
    if (!this.repo) {
      const dir = this.dir();
      if (!dir) return;
      let repo;
      try {
        repo = (await api.gitStatus(dir)).repo;
      } catch (e) {
        if (run === this.historyRun) list.append(el("p", "git-note error", api.asError(e).message));
        return;
      }
      if (run !== this.historyRun) return;
      this.repo = repo;
    }
    if (!this.repo) {
      list.append(el("p", "git-note", "This folder is not in a git repository."));
      return;
    }
    await this.loadMore(list, this.repo, run);
  }

  private async loadMore(list: HTMLElement, repo: string, run: number) {
    list.querySelector(".git-more")?.remove();
    let commits: api.GitCommit[];
    try {
      commits = await api.gitLog(repo, this.loadedSkip, PAGE);
    } catch (e) {
      if (run === this.historyRun) list.append(el("p", "git-note error", api.asError(e).message));
      return;
    }
    // A newer reload owns the list and the page count now.
    if (run !== this.historyRun) return;
    if (this.loadedSkip === 0 && commits.length === 0) {
      list.append(el("p", "git-note", "No commits yet."));
    }
    for (const c of commits) list.append(this.commitRow(c, repo));
    this.loadedSkip += commits.length;
    if (commits.length < PAGE) {
      this.historyDone = true;
    } else {
      const more = el("button", "git-more", "Load older commits");
      more.addEventListener("click", () => void this.loadMore(list, repo, run));
      list.append(more);
    }
  }

  private commitRow(c: api.GitCommit, repo: string): HTMLElement {
    const wrap = el("div", "git-commit");
    const row = el("div", "git-commit-row");
    row.tabIndex = 0;
    row.setAttribute("role", "button");
    row.setAttribute("aria-expanded", "false");
    row.title = `${c.hash}\n${c.author}, ${new Date(c.time * 1000).toLocaleString()}`;
    row.append(
      el("div", "git-subject", c.subject),
      el("div", "git-meta", `${c.short} · ${c.author} · ${relativeTime(c.time)}`),
    );
    const files = el("div", "git-commit-files");
    files.hidden = true;
    let loaded = false;
    const toggle = async () => {
      files.hidden = !files.hidden;
      row.setAttribute("aria-expanded", String(!files.hidden));
      if (files.hidden || loaded) return;
      loaded = true;
      files.append(el("p", "git-note", "Loading…"));
      try {
        const changes = await api.gitCommitFiles(repo, c.hash);
        files.replaceChildren(
          ...changes.map((f) =>
            changeRow(f, () =>
              this.openDiff({ repo, path: f.path, oldPath: f.old_path, rev: c.hash, short: c.short }),
            ),
          ),
        );
        if (changes.length === 0) files.append(el("p", "git-note", "No file changes."));
      } catch (e) {
        loaded = false;
        files.replaceChildren(el("p", "git-note error", api.asError(e).message));
      }
    };
    row.addEventListener("click", () => void toggle());
    row.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        void toggle();
      }
    });
    wrap.append(row, files);
    return wrap;
  }
}
