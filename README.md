# yonder

A small, fast editor for code on remote machines over SSH.

Typing never waits on the network: files are edited locally and only saving
goes to the remote. Nothing is left running, locked, or half-installed on the
remote, and when a connection fails Yonder tells you which step failed and why.

**Status:** milestone 1 of 5 — connect, browse, edit and save. See the
[roadmap](#roadmap).

## Using it

Connect with anything `ssh` accepts as a destination, usually a `Host` alias
from `~/.ssh/config`. Yonder runs your own `ssh` binary, so ProxyJump,
ssh-agent, ControlMaster and short-lived keys or certificates work as they do
in a terminal.

Yonder runs ssh non-interactively (`BatchMode=yes`), so it cannot answer
password or MFA prompts yet. If this works in a terminal, Yonder will connect:

```sh
ssh -o BatchMode=yes <host> true
```

- **Save:** <kbd>Cmd</kbd>+<kbd>S</kbd>. If the file changed on the remote
  since you opened it (a batch job rewrote it, say), Yonder asks before
  overwriting.
- **New files from elsewhere:** the tree re-lists its open folders when the
  window regains focus, or when you press ↻. This works on Lustre and NFS,
  where file-change notifications miss writes from other machines.
- **Lost connection:** open files and unsaved edits stay; press Reconnect.

## How it works

```
 your Mac                                       remote (Linux)
┌──────────────────────────────────┐   ssh    ┌──────────────────────────────┐
│ Yonder.app (Tauri)               │  stdio   │ yonder-agent (static binary) │
│  ├ UI: TypeScript + Monaco       │◄────────►│  file operations             │
│  └ Rust: runs ssh, routes msgs   │ msgpack  │  exits when ssh closes       │
└──────────────────────────────────┘  frames  └──────────────────────────────┘
```

Connecting runs `ssh <host>` once. A one-line `sh` script on the remote
reports the CPU type, then either starts the agent cached in
`~/.cache/yonder/agent-<hash>` or receives it over the same session (about
700 KB, once per Yonder version) and installs it with an atomic rename. There
is no daemon and no lock file.

Saving writes a temporary file next to the original and renames it into
place, keeping permissions. Hard-linked files and files owned by someone else
(group-shared project directories) are written in place instead, so links and
ownership survive.

| Path | What |
|---|---|
| `crates/proto` | Messages and framing shared by both sides |
| `crates/agent` | `yonder-agent`, the remote side |
| `crates/client` | Starts the agent over ssh; request routing |
| `app/src-tauri` | Desktop process: Tauri commands over the client |
| `app/src` | UI: file tree, tabs, Monaco editor |

## Building

You need Rust (via [rustup](https://rustup.rs)) and Node.js 20 or newer.

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
2. **Watching and terminal:** periodic re-listing of open folders and open
   files; terminal tabs.
3. **Git and viewers:** changed files with diffs, commit history, PDF and
   PNG viewers.
4. **Hardening:** automatic reconnect, password and MFA prompts, large-file
   handling, signing.
5. **AI suggestions (optional):** propose small edits, shown as a diff to
   accept or reject.

## License

Apache 2.0
