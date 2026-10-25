# 0.5.0

Added in the autonomous feature cycles of 2026-10-18 to 2026-10-25. Each
feature is also described in the README.

## Git
- The **Changes** tab can **stage** (+), **unstage** (−) and **revert** (↩,
  asks first) a file, **commit** the staged files with a message, and
  **push** (setting the upstream to `origin` the first time). Staged files
  are shown in bold.

## Files and the tree
- Right-click **New File…**, **New Folder…**, **Duplicate…**, **Rename…**,
  **Delete…** (asks first; there is no trash) and **Open in Terminal**.
- A **Collapse All** button above the tree; **Reveal in File Tree**
  (Cmd+Alt+R).
- New agent operations `Rename`, `MakeDir`, `Remove` and `Copy`: the protocol
  is now version 9. None of them replaces an existing file.

## Editing
- **Save All** (Cmd+Alt+S); **Auto Save on Focus Loss** (File menu, off by
  default); `.json` files are formatted as they are saved.
- **Word wrap** (Alt+Z) and **text size** (Cmd+=, Cmd+-, Cmd+0), both
  remembered.
- The status bar shows how many characters are selected.
- **Signature help** (Ctrl+Shift+Space) from the language server.

## Tabs and navigation
- **Reopen Closed Tab** (Cmd+Shift+T), **Close All Tabs** (Cmd+Alt+W),
  **Close Other Tabs** (Cmd+Alt+T), **Next/Previous Tab** (Cmd+Alt+→/←) and
  **Go to Open Tab…** (Cmd+Alt+P).
- **Go to File** and `yonder FILE` accept `FILE:LINE` to open at a line.

## Terminal
- Cmd+K clears the scrollback; Cmd+=, Cmd+- and Cmd+0 change the text size
  (remembered).

## Window
- The window size, the sidebar width and the terminal panel height are
  remembered across restarts.

## Fixes
- Word Wrap, Reopen Closed Tab and Save All did nothing at first: the Rust
  side forwards only menu ids on a list, and they were not on it.
- Alt+Z never reached its menu shortcut on macOS (Option+Z types a
  character), so the editor handles the key itself.
- The end-to-end test `process_and_tcp_streams` was flaky: stderr and stdout
  are separate pipes, so their order is not fixed.

## Not checked
Most of these were built and type-checked but not exercised by hand in the
installed app. Only the agent operations have tests (run on Linux in Docker).
