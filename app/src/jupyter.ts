// Notebooks open in JupyterLab running on the remote, in the workspace
// folder, with the kernel of the nearest venv (from the notebook's folder up
// to the workspace folder), as VS Code does: the venv needs only ipykernel.
// JupyterLab is the venv's, else the one on PATH, else run by uv without
// being added to the project. Its page comes through the agent, so it needs
// no second login or open port. One server per workspace, kept while
// connected, so kernels survive closing a tab.

import { procOpen, tunnelOpen } from "./api";

const quote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/**
 * Started in the workspace folder. The venv's Python becomes the "python3"
 * kernel through a kernel spec in ~/.cache/yonder, which Yonder already
 * uses. Jupyter refuses to be framed unless told the app may frame it. The
 * token is Jupyter's own, read from its log, so it never shows in `ps`.
 */
const script = (notebookDir: string) => `d=${quote(notebookDir)} venv=
while :; do
  for v in .venv venv env; do
    [ -x "$d/$v/bin/python" ] && venv="$d/$v" && break 2
  done
  { [ "$d" = "$PWD" ] || [ "$d" = / ]; } && break
  d=$(dirname "$d")
done
if [ -n "$venv" ]; then
  echo "yonder-jupyter: venv $venv" >&2
  PATH="$venv/bin:$PATH"
  kdir="$HOME/.cache/yonder/jupyter/$(printf %s "$venv" | cksum | cut -d' ' -f1)"
  if "$venv/bin/python" - "$kdir" "$venv" <<'PY'
import importlib.util, json, os, sys
if not importlib.util.find_spec("ipykernel"):
    sys.exit("yonder-jupyter: no ipykernel in " + sys.argv[2] + ", so notebooks get JupyterLab's own Python. uv pip install ipykernel adds it.")
d = os.path.join(sys.argv[1], "kernels", "python3")
os.makedirs(d, exist_ok=True)
name = os.path.basename(os.path.dirname(os.path.abspath(sys.argv[2])))
with open(os.path.join(d, "kernel.json"), "w") as f:
    json.dump({"argv": [sys.executable, "-m", "ipykernel_launcher", "-f", "{connection_file}"],
               "display_name": "Python (" + name + ")", "language": "python"}, f)
PY
  then export JUPYTER_PATH="$kdir\${JUPYTER_PATH:+:$JUPYTER_PATH}"; fi
fi
# In the app's frame the page's cookies count as third-party and WebKit
# drops them, so the kernel's WebSocket must carry the token itself.
cfg="$HOME/.cache/yonder/jupyter/config"
mkdir -p "$cfg/labconfig" && printf '{"appendToken": "true"}\n' > "$cfg/labconfig/page_config.json"
export JUPYTER_CONFIG_PATH="$cfg\${JUPYTER_CONFIG_PATH:+:$JUPYTER_CONFIG_PATH}"
csp='{"headers":{"Content-Security-Policy":"frame-ancestors tauri: http://localhost:*"}}'
set -- --no-browser --ip=127.0.0.1 --port=8888 --port-retries=200 --ServerApp.root_dir="$PWD" --ServerApp.tornado_settings="$csp"
if command -v jupyter-lab >/dev/null 2>&1; then echo "yonder-jupyter: lab" >&2; exec jupyter-lab "$@"; fi
if command -v jupyter-notebook >/dev/null 2>&1; then echo "yonder-jupyter: notebook" >&2; exec jupyter-notebook "$@"; fi
if command -v uv >/dev/null 2>&1; then echo "yonder-jupyter: lab (uv)" >&2; exec uv tool run --from jupyterlab jupyter-lab "$@"; fi
exit 127`;

type Found = { base: string; token: string; lab: boolean };

export class Jupyter {
  private servers = new Map<string, Promise<Found>>();
  /** The end of each server's log, kept after it stops. */
  private logs = new Map<string, string>();

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
      const started: Promise<Found> = this.start(conn, rootDir, dir, key, () => {
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

  /** The end of the workspace's Jupyter log, token hidden; null if none ran. */
  log(host: string, root: string): string | null {
    const log = this.logs.get(`${this.connOf(host)}:${root.replace(/\/$/, "")}`);
    return log === undefined ? null : log.replace(/token=[0-9a-zA-Z]+/g, "token=…");
  }

  private start(conn: number, root: string, dir: string, key: string, ended: () => void): Promise<Found> {
    let log = "";
    let lab = true;
    return new Promise<Found>((resolve, reject) => {
      let done = false;
      procOpen(
        conn,
        root,
        script(dir),
        (data) => {
          log = (log + new TextDecoder().decode(data)).slice(-20000);
          this.logs.set(key, log);
          if (done) return;
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
                ? `No JupyterLab found: none in a .venv, venv or env folder from ${dir} up to ${root}, none on PATH, and no uv to run it. Install jupyterlab in the project's venv.`
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
