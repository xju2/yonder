// LaTeX completion: \cite{…} keys from the .bib files, \ref{…} labels from
// the document and the files it \input's.

import * as monaco from "monaco-editor";
import { listDir, readFile, stat } from "./api";
import { joinPath } from "./tree";

const dirOf = (p: string) => p.slice(0, p.lastIndexOf("/")) || "/";
const split = (s: string) => s.split(",").map((x) => x.trim()).filter(Boolean);

/** Reads a file for as long as its version holds; null when unreadable. */
const cache = new Map<string, { version: string; value: unknown }>();
async function cached<T>(path: string, parse: (text: string) => T): Promise<T | null> {
  try {
    const { version } = await stat(path);
    const hit = cache.get(path);
    if (hit?.version === version) return hit.value as T;
    const text = (await readFile(path)).text;
    if (text === null) return null;
    const value = parse(text);
    cache.set(path, { version, value });
    return value;
  } catch {
    return null;
  }
}

interface Tex {
  labels: string[];
  inputs: string[];
  bibs: string[];
}

function scan(text: string): Tex {
  text = text.replace(/(^|[^\\])%.*$/gm, "$1");
  const all = (re: RegExp) => [...text.matchAll(re)].map((m) => m[1]);
  return {
    labels: all(/\\label\{([^}]+)\}/g),
    inputs: all(/\\(?:input|include|subfile)\{([^}]+)\}/g),
    bibs: [
      ...all(/\\bibliography\{([^}]+)\}/g).flatMap(split),
      ...all(/\\addbibresource(?:\[[^\]]*\])?\{([^}]+)\}/g),
    ],
  };
}

/** The document's labels and bib files, following \input up to a depth. */
async function collect(path: string, text: string, depth = 0, seen = new Set<string>()): Promise<Tex> {
  const out = scan(text);
  const dir = dirOf(path);
  out.bibs = out.bibs.map((b) => joinPath(dir, b.endsWith(".bib") ? b : `${b}.bib`));
  if (depth >= 4) return out;
  for (const f of out.inputs) {
    const p = joinPath(dir, f.endsWith(".tex") ? f : `${f}.tex`);
    if (seen.has(p)) continue;
    seen.add(p);
    const sub = await readFile(p).catch(() => null);
    if (sub?.text == null) continue;
    const r = await collect(p, sub.text, depth + 1, seen);
    out.labels.push(...r.labels);
    out.bibs.push(...r.bibs);
  }
  return out;
}

interface Entry {
  key: string;
  detail: string;
}

const parseBib = (text: string): Entry[] =>
  [...text.matchAll(/@(\w+)\s*[{(]\s*([^\s,]+)\s*,([\s\S]*?)(?=\n\s*@|$)/g)]
    .filter((m) => !/^(comment|string|preamble)$/i.test(m[1]))
    .map((m) => {
      const field = (n: string) => {
        const h = new RegExp(`\\b${n}\\s*=\\s*`, "i").exec(m[3]);
        if (!h) return "";
        const s = m[3].slice(h.index + h[0].length);
        if (s[0] === "{") {
          let d = 0;
          for (let i = 0; i < s.length; i++) {
            d += s[i] === "{" ? 1 : s[i] === "}" ? -1 : 0;
            if (!d) return s.slice(1, i);
          }
          return "";
        }
        return s.match(/^"([^"]*)"|^([^,\s}]+)/)?.slice(1).find(Boolean) ?? "";
      };
      const clean = (s: string) => s.replace(/\s+/g, " ").replace(/[{}]/g, "").trim();
      const author = clean(field("author")).split(/ and /)[0];
      return { key: m[2], detail: [author, field("year"), clean(field("title"))].filter(Boolean).join(" · ") };
    });

async function bibEntries(files: string[], dir: string): Promise<Entry[]> {
  if (!files.length) {
    files = (await listDir(dir).catch(() => []))
      .filter((e) => e.kind === "file" && e.name.endsWith(".bib"))
      .map((e) => joinPath(dir, e.name));
  }
  const seen = new Map<string, Entry>();
  for (const f of files) for (const e of (await cached(f, parseBib)) ?? []) seen.set(e.key, e);
  return [...seen.values()];
}

const CITE = /\\(?:[a-zA-Z]*cite[a-zA-Z]*|nocite)\*?(?:\[[^\]]*\]){0,2}\{([^{}]*)$/;
const REF = /\\(?:[a-zA-Z]*ref|cref|Cref|autoref|pageref|nameref)\*?\{([^{}]*)$/;

monaco.languages.registerCompletionItemProvider("latex", {
  triggerCharacters: ["{", ","],
  async provideCompletionItems(model, pos) {
    const before = model.getValueInRange(new monaco.Range(pos.lineNumber, 1, pos.lineNumber, pos.column));
    const cite = CITE.exec(before);
    const ref = cite ? null : REF.exec(before);
    const arg = (cite ?? ref)?.[1];
    if (arg === undefined) return { suggestions: [] };

    const typed = arg.slice(arg.lastIndexOf(",") + 1).trimStart();
    const range = new monaco.Range(pos.lineNumber, pos.column - typed.length, pos.lineNumber, pos.column);
    const path = model.uri.path;
    const tex = await collect(path, model.getValue());
    const kind = monaco.languages.CompletionItemKind;
    const suggestions = cite
      ? (await bibEntries(tex.bibs, dirOf(path))).map((e) => ({
          label: { label: e.key, description: e.detail },
          kind: kind.Reference,
          insertText: e.key,
          filterText: `${e.key} ${e.detail}`,
          range,
        }))
      : [...new Set(tex.labels)].map((l) => ({ label: l, kind: kind.Reference, insertText: l, range }));
    return { suggestions };
  },
});
