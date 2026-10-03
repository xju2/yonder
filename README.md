# yonder

A small, fast editor for code on remote machines over SSH.

Typing never waits on the network: files are edited locally and only saving
goes to the remote. Nothing is left running, locked, or half-installed on the
remote, and when a connection fails Yonder tells you which step failed and why.

**Status:** connect, browse, edit and save; view images and PDFs; terminals;
git changes and history. See the
[roadmap](#roadmap).

## Using it

Connect with anything `ssh` accepts as a destination, usually a `Host` alias
from `~/.ssh/config`. Yonder runs your own `ssh` binary, so ProxyJump,
ssh-agent, ControlMaster and short-lived keys or certificates work as they do
in a terminal.

When ssh asks something (a password, a password plus one-time code, or
whether to trust an unknown host key), Yonder shows the question in a
dialog. Cancel ends that login attempt.

- **Save:** <kbd>Cmd</kbd>+<kbd>S</kbd>. If the file changed on the remote
  since you opened it (a batch job rewrote it, say), Yonder asks before
  overwriting.
- **Files changed elsewhere:** when you come back to the window or switch
  tabs, an open file that a job or `git pull` rewrote is reloaded. If it has
  no unsaved edits, the new text replaces it and <kbd>Cmd</kbd>+<kbd>Z</kbd>
  brings yours back. If it has unsaved edits, the status bar tells you, and
  saving asks first.
- **Workspaces:** each is one folder on one host. <kbd>Cmd</kbd>+<kbd>Alt</kbd>+<kbd>O</kbd>
  (or clicking the host:folder in the status bar) switches to a recent one;
  <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>N</kbd> opens a new one. The workspace you
  leave keeps its tabs, unsaved edits, terminals and expanded folders for
  when you come back, and each host's connection stays open, so switching
  needs no new login. A workspace's open files are remembered across
  restarts and reopen the first time you switch to it.
- **Search:** <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>F</kbd> or the sidebar's
  **Search** tab finds text in every file of the folder (with match case,
  whole word and regular expression toggles; ⋯ adds files to include and
  exclude, such as `*.py, ./src` or `build`). It runs `git grep` on the
  remote, so binary and git-ignored files are skipped, inside a repository
  or not. Click a line to open it there.
- **Code intelligence:** in Python and C/C++ files, <kbd>F12</kbd> or
  <kbd>Cmd</kbd>+click goes to a definition (opening its file), hovering shows
  types and docs, problems are underlined, and <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd>
  lists the file's symbols. Yonder uses the language server already on the
  remote: basedpyright, pyright or pylsp from the folder's `.venv`, `venv` or
  `env` first, then `PATH`; clangd from `PATH` (it reads
  `compile_commands.json`). With none, the status bar says what to install.
- **Double-click a tab** to show its file in the tree.
- **New files from elsewhere:** the tree re-lists its open folders when the
  window regains focus, or when you press ↻. This works on Lustre and NFS,
  where file-change notifications miss writes from other machines.
- **Images and PDFs** (PNG, JPEG, GIF, WebP, SVG, PDF, …) open in a viewer.
  When you come back to the window, or press ↻, a file that changed on the
  remote (a regenerated plot, say) is reloaded, keeping zoom and position.
- **Git:** the sidebar's **Changes** tab lists uncommitted files (staged and
  not) against the last commit; **History** lists commits, and expanding one
  shows its files. Clicking a file opens a side-by-side diff. It is read-only:
  commit from the terminal. Git runs without taking locks, so browsing never
  gets in the way of your own git commands.
- **Terminal:** <kbd>Ctrl</kbd>+<kbd>`</kbd> or the Terminal button opens your
  login shell on the remote, in the open folder; + adds more. A program that
  prints endlessly is paused rather than flooding the window, so
  <kbd>Ctrl</kbd>+<kbd>C</kbd> always answers at once. Terminals end with the
  connection; use `tmux` inside one if a session must survive.
- **`yonder FILE`** in a Yonder terminal opens the file in this window. If
  your login files reset `PATH`, add `~/.cache/yonder/bin` back to it; inside
  `tmux`, it needs `set -g allow-passthrough on`.
- **Lost connection:** open files and unsaved edits stay, and Yonder
  reconnects on its own: after 1, 2, 5, 10, 20, then every 30 seconds, and at
  once when the Mac comes back online. It stops when a person is needed (a
  cancelled password, a rejected host key); press Try again then.
- **Quitting** with unsaved edits, by <kbd>Cmd</kbd>+<kbd>Q</kbd>, the Dock or
  closing the window, asks first.

## How it works

```
 your Mac                                       remote (Linux)
┌──────────────────────────────────┐   ssh    ┌──────────────────────────────┐
│ Yonder.app (Tauri)               │  stdio   │ yonder-agent (static binary) │
│  ├ UI: TypeScript + Monaco       │◄────────►│  files, terminals, git       │
│  └ Rust: runs ssh, routes msgs   │ msgpack  │  exits when ssh closes       │
└──────────────────────────────────┘  frames  └──────────────────────────────┘
```

Connecting runs `ssh <host>` once. A one-line `sh` script on the remote
reports the CPU type, then either starts the agent cached in
`~/.cache/yonder/agent-<hash>` or receives it over the same session (about
1 MB, once per Yonder version) and installs it with an atomic rename. There
is no daemon and no lock file.

Saving writes a temporary file next to the original and renames it into
place, keeping permissions. Hard-linked files and files owned by someone else
(group-shared project directories) are written in place instead, so links and
ownership survive.

| Path | What |
|---|---|
| `crates/proto` | Messages and framing shared by both sides |
| `crates/agent` | `yonder-agent`, the remote side: files, and shells on pseudo-terminals |
| `crates/client` | Starts the agent over ssh; request routing; git parsing |
| `app/src-tauri` | Desktop process: Tauri commands over the client |
| `app/src` | UI: file tree, tabs, Monaco editor, image and PDF viewers ([pdf.js](https://mozilla.github.io/pdf.js/)), terminals ([xterm.js](https://xtermjs.org/)) |

## Building

You need Rust (via [rustup](https://rustup.rs)) and Node.js 20.19+ or 22.12+.

```sh
scripts/build-agents.sh        # static Linux agents for x86_64 and aarch64
cd app
npm ci
npx tauri dev                  # run from source
npx tauri build                # Yonder.app and a .dmg in target/release/bundle
```

The agents cross-compile from macOS with nothing beyond `rustup`: Rust ships
the musl C library and its own linker (see `.cargo/config.toml`).

Every pull request also builds a macOS disk image; download it from the
**Yonder-macos-arm64** artifact on the workflow run. The app is signed ad hoc
but not notarized by Apple, so macOS blocks it on first launch. After copying
it to Applications, run once:

```sh
xattr -dr com.apple.quarantine /Applications/Yonder.app
```

### Debugging a connection without the app

```sh
cargo run -p yonder-client --example probe -- <host> [folder]
```

prints every connection step and lists the folder.

### Tests

```sh
cargo test --workspace
```

The end-to-end tests in `crates/agent/tests` run the real bootstrap script and
agent through a stand-in for `ssh`.

## Roadmap

1. **Connect and edit** — done.
2. **Watching and terminal** — done: terminal tabs; the tree and open files
   are re-read when the window regains focus.
3. **Git and viewers** — done: changed files and history with diffs; image
   and PDF viewers.
4. **Hardening:** automatic reconnect, password and MFA prompts, and asking
   before quitting with unsaved edits are done; signing and notarization next.
5. **AI suggestions (optional):** propose small edits, shown as a diff to
   accept or reject.

## License

Apache 2.0
