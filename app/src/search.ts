// The sidebar's Search view: find text in every file of the open folder.
// The remote runs `git grep`, which skips binary and git-ignored files and
// works outside repositories too.

import * as api from "./api";
import type { Place } from "./editor";
import { joinPath } from "./tree";

/** Text shown around a match on a long line. */
const BEFORE = 40;
const SHOWN = 300;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

/** The search as a JavaScript pattern, to find matches within a line. */
export function matcher(query: string, regex: boolean, caseSensitive: boolean, word: boolean) {
  let src = regex ? query : query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  if (word) src = `\\b(?:${src})\\b`;
  try {
    return new RegExp(src, caseSensitive ? "g" : "gi");
  } catch {
    // git's regular expressions are not JavaScript's; the line still shows.
    return null;
  }
}

/** Where each match is in `text`, as [start, end) pairs. */
export function spans(text: string, re: RegExp | null): [number, number][] {
  if (!re) return [];
  const out: [number, number][] = [];
  for (const m of text.matchAll(re)) {
    if (m[0].length === 0) break;
    out.push([m.index!, m.index! + m[0].length]);
  }
  return out;
}

export class SearchPanel {
  private input: HTMLInputElement;
  private toggles: Record<"caseSensitive" | "word" | "regex", HTMLButtonElement>;
  private summary: HTMLElement;
  private results: HTMLElement;
  /** Bumped by each search; older answers are dropped. */
  private run = 0;

  constructor(
    view: HTMLElement,
    /** The open folder. */
    private dir: () => string | null,
    private onOpen: (path: string, at: Place) => void,
  ) {
    this.input = el("input");
    this.input.type = "search";
    this.input.placeholder = "Search";
    this.input.setAttribute("aria-label", "Search in the folder");
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") void this.search();
    });
    const toggle = (label: string, title: string) => {
      const b = el("button", "search-toggle", label);
      b.title = title;
      b.setAttribute("aria-label", title);
      b.setAttribute("aria-pressed", "false");
      b.addEventListener("click", () => {
        b.setAttribute("aria-pressed", String(b.getAttribute("aria-pressed") !== "true"));
        if (this.input.value) void this.search();
      });
      return b;
    };
    this.toggles = {
      caseSensitive: toggle("Aa", "Match case"),
      word: toggle("ab", "Match whole word"),
      regex: toggle(".*", "Use regular expression"),
    };
    const box = el("div", "search-box");
    box.append(this.input, ...Object.values(this.toggles));
    this.summary = el("p", "git-note");
    this.summary.setAttribute("role", "status");
    this.results = el("div", "git-body");
    view.append(box, this.summary, this.results);
  }

  /** Put the keyboard in the search box, starting with `text` if given. */
  focus(text: string) {
    if (text) this.input.value = text;
    this.input.focus();
    this.input.select();
  }

  /** Forget the results: they belong to another folder. */
  reset() {
    this.run++;
    this.summary.textContent = "";
    this.results.replaceChildren();
  }

  private on(name: keyof SearchPanel["toggles"]) {
    return this.toggles[name].getAttribute("aria-pressed") === "true";
  }

  private async search() {
    const dir = this.dir();
    const query = this.input.value;
    if (!dir) return;
    this.reset();
    if (!query) return;
    const run = this.run;
    const [regex, caseSensitive, word] = [this.on("regex"), this.on("caseSensitive"), this.on("word")];
    this.summary.textContent = "Searching…";
    let found;
    try {
      found = await api.search(dir, query, regex, caseSensitive, word);
    } catch (e) {
      if (run !== this.run) return;
      this.summary.textContent = api.asError(e).message;
      this.summary.classList.add("error");
      return;
    }
    if (run !== this.run) return;
    this.summary.classList.remove("error");
    const byFile = new Map<string, api.SearchMatch[]>();
    for (const m of found.matches) {
      if (!byFile.has(m.path)) byFile.set(m.path, []);
      byFile.get(m.path)!.push(m);
    }
    const n = found.matches.length;
    this.summary.textContent =
      n === 0
        ? "No results."
        : `${n.toLocaleString()} result${n === 1 ? "" : "s"} in ${byFile.size.toLocaleString()} file${byFile.size === 1 ? "" : "s"}` +
          (found.truncated ? ". There are more: narrow the search." : "");
    const re = matcher(query, regex, caseSensitive, word);
    for (const [path, matches] of byFile) this.results.append(this.fileGroup(dir, path, matches, re));
  }

  private fileGroup(dir: string, path: string, matches: api.SearchMatch[], re: RegExp | null) {
    const group = el("div", "search-file");
    const head = el("div", "git-file");
    head.tabIndex = 0;
    head.setAttribute("role", "button");
    head.setAttribute("aria-expanded", "true");
    head.title = path;
    const name = path.split("/").pop()!;
    const folder = path.slice(0, path.length - name.length).replace(/\/$/, "");
    head.append(
      el("span", "twisty", "▾"),
      el("span", "git-name", name),
      el("span", "git-dir", folder),
      el("span", "search-count", String(matches.length)),
    );
    const lines = el("div");
    const fold = () => {
      lines.hidden = !lines.hidden;
      head.setAttribute("aria-expanded", String(!lines.hidden));
      head.firstElementChild!.textContent = lines.hidden ? "▸" : "▾";
    };
    head.addEventListener("click", fold);
    head.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        fold();
      }
    });
    for (const m of matches) lines.append(this.matchRow(joinPath(dir, path), m, re));
    group.append(head, lines);
    return group;
  }

  private matchRow(path: string, m: api.SearchMatch, re: RegExp | null) {
    const row = el("div", "search-line");
    row.tabIndex = 0;
    row.setAttribute("role", "button");
    row.title = `Line ${m.line}`;
    const found = spans(m.text, re);
    // Long lines (minified files) show the part around the first match;
    // indentation is left out.
    const firstAt = found[0]?.[0] ?? 0;
    const indent = m.text.length - m.text.trimStart().length;
    const start = firstAt > BEFORE ? firstAt - BEFORE : Math.min(indent, firstAt);
    const end = Math.min(m.text.length, start + SHOWN);
    if (start > 0) row.append("…");
    let at = start;
    for (const [a, b] of found) {
      if (a >= end) break;
      if (b <= at) continue;
      row.append(m.text.slice(at, Math.max(a, at)), el("mark", undefined, m.text.slice(Math.max(a, at), Math.min(b, end))));
      at = Math.min(b, end);
    }
    row.append(m.text.slice(at, end).trimEnd());
    const first = found[0] ?? [0, 0];
    const open = () => this.onOpen(path, { line: m.line, column: first[0] + 1, length: first[1] - first[0] });
    row.addEventListener("click", open);
    row.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        open();
      }
    });
    return row;
  }
}
