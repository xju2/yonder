// Terminal tabs in a panel below the editor. Each runs the remote login shell
// on a pseudo-terminal owned by the agent; xterm.js draws it. Output streams
// in as raw bytes, and keystrokes go out without waiting for an answer.

import { Channel } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal, type ITheme } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import * as api from "./api";

const LIGHT: ITheme = {
  background: "#ffffff",
  foreground: "#1d1d1f",
  cursor: "#1d1d1f",
  cursorAccent: "#ffffff",
  selectionBackground: "#b4d5fe",
};
const DARK: ITheme = {
  background: "#1e1e1e",
  foreground: "#e4e4e6",
  cursor: "#e4e4e6",
  cursorAccent: "#1e1e1e",
  selectionBackground: "#264f78",
};
const dark = window.matchMedia("(prefers-color-scheme: dark)");

/** Acknowledge drawn output in batches of about this many bytes. */
const ACK_BATCH = 64 * 1024;

interface Term {
  term: Terminal;
  fit: FitAddon;
  /** null until the shell has started. */
  pty: number | null;
  alive: boolean;
  view: HTMLElement;
  tab: HTMLElement;
  /** Bytes drawn but not yet acknowledged to the remote. */
  unacked: number;
  ackTimer: number;
  /** The tab was closed; its xterm is disposed. */
  closed: boolean;
}

export class TerminalPanel {
  private terms: Term[] = [];
  private active: Term | null = null;
  private byPty = new Map<number, Term>();
  private count = 0;

  constructor(
    private panel: HTMLElement,
    private sash: HTMLElement,
    private tabsEl: HTMLElement,
    private body: HTMLElement,
    /** Where new shells start: the open folder. */
    private cwd: () => string | null,
    /** `yonder FILE` was run in a terminal. */
    private onOpen: (path: string) => void,
  ) {
    void api.onPtyExit((e) => this.exited(e.pty, e.code));
    dark.addEventListener("change", () => {
      for (const t of this.terms) t.term.options.theme = dark.matches ? DARK : LIGHT;
    });
    new ResizeObserver(() => this.fitActive()).observe(this.body);
  }

  get visible() {
    return !this.panel.hidden;
  }

  toggle() {
    if (this.visible) this.hide();
    else this.show();
  }

  show() {
    this.panel.hidden = false;
    this.sash.hidden = false;
    if (this.terms.length === 0) void this.create();
    else {
      this.fitActive();
      this.active?.term.focus();
    }
  }

  hide() {
    this.panel.hidden = true;
    this.sash.hidden = true;
  }

  async create() {
    this.panel.hidden = false;
    this.sash.hidden = false;
    const term = new Terminal({
      fontFamily: '"IBM Plex Mono", "SF Mono", Menlo, Monaco, "DejaVu Sans Mono", monospace',
      fontSize: 13,
      cursorBlink: true,
      scrollback: 10_000,
      // Option works as Meta, as most shells and editors expect.
      macOptionIsMeta: true,
      theme: dark.matches ? DARK : LIGHT,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    // `yonder FILE` in the shell prints ESC ] 7777 ; open ; PATH BEL.
    term.parser.registerOscHandler(7777, (data) => {
      if (data.startsWith("open;/")) this.onOpen(data.slice("open;".length));
      return true;
    });
    // Let the show/hide shortcut through instead of sending it to the shell.
    term.attachCustomKeyEventHandler((e) => !(e.ctrlKey && e.key === "`"));

    const view = document.createElement("div");
    view.className = "term-view";
    this.body.append(view);
    const t: Term = {
      term,
      fit,
      pty: null,
      alive: false,
      view,
      tab: this.buildTab(),
      unacked: 0,
      ackTimer: 0,
      closed: false,
    };
    this.terms.push(t);
    this.select(t);
    term.open(view);
    fit.fit();

    const output = new Channel<ArrayBuffer>();
    // Acknowledge output once xterm has drawn it: the remote pauses a
    // program that gets too far ahead, as a local terminal would.
    output.onmessage = (bytes) => {
      if (!t.closed) term.write(new Uint8Array(bytes), () => this.drawn(t, bytes.byteLength));
    };
    const cols = term.cols;
    const rows = term.rows;
    let opened: api.PtyOpened;
    try {
      opened = await api.ptyOpen(cols, rows, this.cwd(), output);
    } catch (e) {
      if (t.closed) return;
      const msg = api.asError(e).message;
      term.write(`\x1b[31mCould not start a terminal: ${msg}\x1b[0m\r\n`);
      this.markDead(t);
      return;
    }
    t.pty = opened.pty;
    if (t.closed) {
      // Closed while the shell was starting: hang it up rather than leave
      // it running on the remote.
      if (!opened.exited) api.ptyClose(opened.pty).catch(() => {});
      return;
    }
    if (opened.exited) {
      this.showExit(t, opened.code);
      return;
    }
    t.alive = true;
    this.byPty.set(opened.pty, t);
    this.flushAck(t);
    term.onData((data) => {
      if (t.alive && t.pty !== null) api.ptyWrite(t.pty, data).catch(() => this.markDead(t));
    });
    term.onResize(({ cols, rows }) => {
      if (t.alive && t.pty !== null) api.ptyResize(t.pty, cols, rows).catch(() => {});
    });
    // The panel may have been resized while the shell started.
    if (term.cols !== cols || term.rows !== rows) {
      api.ptyResize(t.pty!, term.cols, term.rows).catch(() => {});
    }
    if (this.active === t) term.focus();
  }

  /** The connection is gone: running shells ended with it. */
  disconnected() {
    for (const t of this.byPty.values()) {
      t.term.write("\r\n\x1b[2m[connection lost; open a new terminal after reconnecting]\x1b[0m\r\n");
      this.markDead(t);
    }
    this.byPty.clear();
  }

  private buildTab(): HTMLElement {
    const tab = document.createElement("div");
    tab.className = "term-tab";
    const label = document.createElement("span");
    label.textContent = `shell ${++this.count}`;
    const close = document.createElement("button");
    close.className = "close";
    close.textContent = "×";
    close.title = "Close terminal";
    close.setAttribute("aria-label", "Close terminal");
    tab.append(label, close);
    tab.addEventListener("click", () => {
      const t = this.terms.find((x) => x.tab === tab);
      if (t) this.select(t);
    });
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      const t = this.terms.find((x) => x.tab === tab);
      if (t) this.close(t);
    });
    this.tabsEl.append(tab);
    return tab;
  }

  /** Close the active terminal if it has the keyboard; false otherwise. */
  closeFocused(): boolean {
    if (!this.active || !this.panel.contains(document.activeElement)) return false;
    this.close(this.active);
    return true;
  }

  private select(t: Term) {
    this.active = t;
    for (const x of this.terms) {
      x.view.hidden = x !== t;
      x.tab.classList.toggle("active", x === t);
    }
    this.fitActive();
    t.term.focus();
  }

  private close(t: Term) {
    t.closed = true;
    clearTimeout(t.ackTimer);
    if (t.alive && t.pty !== null) api.ptyClose(t.pty).catch(() => {});
    if (t.pty !== null) this.byPty.delete(t.pty);
    const i = this.terms.indexOf(t);
    this.terms.splice(i, 1);
    t.term.dispose();
    t.view.remove();
    t.tab.remove();
    const next = this.terms[Math.min(i, this.terms.length - 1)];
    if (next) this.select(next);
    else {
      this.active = null;
      this.hide();
    }
  }

  private exited(pty: number, code: number | null) {
    const t = this.byPty.get(pty);
    if (!t) return;
    this.byPty.delete(pty);
    this.showExit(t, code);
  }

  private showExit(t: Term, code: number | null) {
    const how = code === null ? "" : ` with code ${code}`;
    t.term.write(`\r\n\x1b[2m[process exited${how}]\x1b[0m\r\n`);
    this.markDead(t);
  }

  private drawn(t: Term, n: number) {
    t.unacked += n;
    if (t.unacked >= ACK_BATCH) this.flushAck(t);
    else if (!t.ackTimer) t.ackTimer = window.setTimeout(() => this.flushAck(t), 50);
  }

  private flushAck(t: Term) {
    clearTimeout(t.ackTimer);
    t.ackTimer = 0;
    // Output that arrived before the id was known is acknowledged later.
    if (!t.alive || t.pty === null || t.unacked === 0) return;
    api.ptyAck(t.pty, t.unacked).catch(() => {});
    t.unacked = 0;
  }

  private markDead(t: Term) {
    t.alive = false;
    t.tab.classList.add("dead");
  }

  private fitActive() {
    const t = this.active;
    if (!t || !this.visible || t.view.clientHeight === 0) return;
    try {
      t.fit.fit();
    } catch {
      // Not laid out yet.
    }
  }
}
