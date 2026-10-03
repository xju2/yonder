// Notebooks open in Jupyter running on the remote, in the workspace folder:
// the Jupyter of the nearest venv first (from the notebook's folder up to
// the workspace folder), then the one on PATH. Its page comes
// through the agent, so it needs no second login or open port. One server
// per workspace, kept while connected, so kernels survive closing a tab.

import { procOpen, tunnelOpen } from "./api";

const quote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/**
 * Started in the workspace folder. Jupyter refuses to be framed unless told
 * the app may frame it. The token is Jupyter's own, read from its log, so it
 * never shows in `ps`.
 */
const script = (notebookDir: string) => `d=${quote(notebookDir)}
while :; do
  for v in .venv venv env; do
    [ -x "$d/$v/bin/jupyter" ] && PATH="$d/$v/bin:$PATH" && echo "yonder-jupyter: $d/$v" >&2 && break 2
  done
  { [ "$d" = "$PWD" ] || [ "$d" = / ]; } && break
  d=$(dirname "$d")
done
csp='{"headers":{"Content-Security-Policy":"frame-ancestors tauri: http://localhost:*"}}'
set -- --no-browser --ip=127.0.0.1 --port=8888 --port-retries=200 --ServerApp.root_dir="$PWD" --ServerApp.tornado_settings="$csp"
if command -v jupyter-lab >/dev/null 2>&1; then echo "yonder-jupyter: lab" >&2; exec jupyter-lab "$@"; fi
if command -v jupyter-notebook >/dev/null 2>&1; then echo "yonder-jupyter: notebook" >&2; exec jupyter-notebook "$@"; fi
command -v jupyter >/dev/null 2>&1 && echo "$(command -v jupyter) has neither JupyterLab nor Notebook: pip install jupyterlab" >&2 && exit 3
exit 127`;

type Found = { base: string; token: string; lab: boolean };

export class Jupyter {
  private servers = new Map<string, Promise<Found>>();

  constructor(private connOf: (host: string) => number | null) {}

  /** The page showing notebook `path` of workspace `root` on `host`. */
  async page(host: string, root: string, path: string): Promise<string> {
    const rootDir = root.replace(/\/$/, "");
    if (!path.startsWith(rootDir + "/")) {
      throw new Error("Only notebooks inside the workspace folder open in Jupyter.");
    }
    const conn = this.connOf(host);
    if (!conn) throw new Error("Not connected.");
    const key = `${conn}:${rootDir}`;
    let s = this.servers.get(key);
    if (!s) {
      // ponytail: one Jupyter per workspace, from the venv nearest the first
      // notebook opened; key by venv if notebooks in one folder need several.
      const dir = path.slice(0, path.lastIndexOf("/"));
      const started: Promise<Found> = this.start(conn, rootDir, dir, () => {
        if (this.servers.get(key) === started) this.servers.delete(key);
      });
      this.servers.set(key, (s = started));
    }
    let found;
    try {
      found = await s;
    } catch (e) {
      if (this.servers.get(key) === s) this.servers.delete(key);
      throw e;
    }
    const rel = path.slice(rootDir.length + 1).split("/").map(encodeURIComponent).join("/");
    const local = await tunnelOpen(conn, Number(new URL(found.base).port));
    return `http://127.0.0.1:${local}/${found.lab ? "lab/tree" : "notebooks"}/${rel}?token=${found.token}`;
  }

  private start(conn: number, root: string, dir: string, ended: () => void): Promise<Found> {
    let log = "";
    let lab = true;
    return new Promise<Found>((resolve, reject) => {
      let done = false;
      procOpen(
        conn,
        root,
        script(dir),
        (data) => {
          if (done) return;
          log = (log + new TextDecoder().decode(data)).slice(-8000);
          if (/yonder-jupyter: notebook/.test(log)) lab = false;
          const m = /http:\/\/(?:127\.0\.0\.1|localhost):(\d+)\/\S*?[?&]token=([0-9a-zA-Z]+)/.exec(log);
          if (m) {
            done = true;
            resolve({ base: `http://127.0.0.1:${m[1]}/`, token: m[2], lab });
          }
        },
        (code) => {
          ended();
          if (done) return;
          done = true;
          const last = log.trim().split("\n").slice(-3).join("\n");
          reject(
            new Error(
              code === 127
                ? `No Jupyter found: none in a .venv, venv or env folder from ${dir} up to ${root}, and none on PATH. Install jupyterlab in the project's venv (pip install jupyterlab).`
                : `Jupyter stopped before it was ready.${last ? `\n\n${last}` : ""}`,
            ),
          );
        },
      ).catch((e) => {
        done = true;
        ended();
        reject(new Error(String(e?.message ?? e)));
      });
    });
  }
}
