// Read-only viewers for images and PDFs. Each viewer owns its DOM and can be
// re-loaded with new bytes when the file changes on the remote (a plot that a
// job regenerated), keeping the zoom and scroll position.

import type { PDFDocumentProxy, RenderTask } from "pdfjs-dist";

const IMAGE_TYPES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  avif: "image/avif",
  bmp: "image/bmp",
  ico: "image/x-icon",
  svg: "image/svg+xml",
};

export type ViewKind = "image" | "pdf";

const extension = (path: string) => path.split("/").pop()!.split(".").pop()!.toLowerCase();

export function viewKindFor(path: string): ViewKind | null {
  const ext = extension(path);
  if (ext === "pdf") return "pdf";
  if (ext in IMAGE_TYPES) return "image";
  return null;
}

export interface Viewer {
  readonly el: HTMLElement;
  /** Show `bytes`, replacing what was shown before. */
  load(bytes: ArrayBuffer): Promise<void>;
  /** A short description for the status bar, e.g. "1200 × 800 px". */
  info(): string;
  dispose(): void;
}

export function createViewer(kind: ViewKind, path: string, onReload: () => void): Viewer {
  return kind === "pdf" ? new PdfViewer(onReload) : new ImageViewer(path, onReload);
}

function button(label: string, title: string, onClick: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

function toolbar(info: HTMLElement, ...controls: HTMLElement[]): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "viewer-toolbar";
  const spacer = document.createElement("span");
  spacer.className = "spacer";
  bar.append(info, spacer, ...controls);
  return bar;
}

// ---- images

class ImageViewer implements Viewer {
  readonly el = document.createElement("div");
  private img = document.createElement("img");
  private url: string | null = null;
  private infoEl = document.createElement("span");
  private fitBtn: HTMLButtonElement;

  constructor(
    private path: string,
    onReload: () => void,
  ) {
    this.el.className = "viewer image-viewer fit";
    const stage = document.createElement("div");
    stage.className = "image-stage";
    stage.append(this.img);
    // Clicking the image toggles between fitting the window and actual size.
    this.img.addEventListener("click", () => this.toggleFit());
    this.fitBtn = button("Actual size", "Toggle between fit to window and actual size", () =>
      this.toggleFit(),
    );
    this.el.append(
      toolbar(this.infoEl, this.fitBtn, button("↻", "Reload from the remote", onReload)),
      stage,
    );
  }

  private toggleFit() {
    const fit = this.el.classList.toggle("fit");
    this.fitBtn.textContent = fit ? "Actual size" : "Fit";
  }

  async load(bytes: ArrayBuffer) {
    const type = IMAGE_TYPES[extension(this.path)] ?? "application/octet-stream";
    const url = URL.createObjectURL(new Blob([bytes], { type }));
    const loaded = new Promise<void>((resolve, reject) => {
      this.img.onload = () => resolve();
      this.img.onerror = () => reject(new Error("the image could not be decoded"));
    });
    this.img.src = url;
    try {
      await loaded;
    } finally {
      if (this.url) URL.revokeObjectURL(this.url);
      this.url = url;
    }
    this.infoEl.textContent = this.info();
  }

  info() {
    const { naturalWidth: w, naturalHeight: h } = this.img;
    return w ? `${w} × ${h} px` : "";
  }

  dispose() {
    if (this.url) URL.revokeObjectURL(this.url);
  }
}

// ---- PDF

type PdfJs = typeof import("pdfjs-dist");
let pdfjsPromise: Promise<PdfJs> | null = null;

/** Load pdf.js on first use; it is large and most sessions never need it. */
function loadPdfJs(): Promise<PdfJs> {
  pdfjsPromise ??= Promise.all([
    import("pdfjs-dist"),
    import("pdfjs-dist/build/pdf.worker.min.mjs?url"),
  ]).then(([pdfjs, worker]) => {
    pdfjs.GlobalWorkerOptions.workerSrc = worker.default;
    return pdfjs;
  });
  return pdfjsPromise;
}

/** Fonts, character maps and decoders pdf.js fetches on demand; see scripts/copy-pdfjs.mjs. */
const assets = (dir: string) => `${import.meta.env.BASE_URL}pdfjs/${dir}/`;

const ZOOMS = [0.5, 0.67, 0.8, 1, 1.25, 1.5, 2, 3, 4];

class PdfViewer implements Viewer {
  readonly el = document.createElement("div");
  private pagesEl = document.createElement("div");
  private infoEl = document.createElement("span");
  private zoomEl = document.createElement("span");
  private doc: PDFDocumentProxy | null = null;
  /** null means "fit the page width to the window". */
  private zoom: number | null = null;
  private scale = 1;
  private pages: { el: HTMLElement; rendered: number; task: RenderTask | null }[] = [];
  private observer: IntersectionObserver;
  private resize: ResizeObserver;
  private generation = 0;

  constructor(onReload: () => void) {
    this.el.className = "viewer pdf-viewer";
    this.pagesEl.className = "pdf-pages";
    this.zoomEl.className = "zoom";
    this.el.append(
      toolbar(
        this.infoEl,
        button("−", "Zoom out", () => this.step(-1)),
        this.zoomEl,
        button("+", "Zoom in", () => this.step(1)),
        button("Fit width", "Fit the page width to the window", () => this.setZoom(null)),
        button("↻", "Reload from the remote", onReload),
      ),
      this.pagesEl,
    );
    // Render pages only as they scroll into view; a thesis has hundreds.
    this.observer = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) void this.renderPage(Number((e.target as HTMLElement).dataset.page));
        }
      },
      { root: this.pagesEl, rootMargin: "100% 0px" },
    );
    let lastWidth = 0;
    this.resize = new ResizeObserver(() => {
      const w = this.pagesEl.clientWidth;
      if (this.zoom === null && w > 0 && w !== lastWidth) {
        lastWidth = w;
        void this.layout();
      }
    });
    this.resize.observe(this.pagesEl);
  }

  async load(bytes: ArrayBuffer) {
    const pdfjs = await loadPdfJs();
    const doc = await pdfjs.getDocument({
      data: new Uint8Array(bytes),
      cMapUrl: assets("cmaps"),
      cMapPacked: true,
      standardFontDataUrl: assets("standard_fonts"),
      wasmUrl: assets("wasm"),
      iccUrl: assets("iccs"),
    }).promise;
    // Keep the reading position across reloads of a regenerated file.
    const ratio = this.pagesEl.scrollHeight
      ? this.pagesEl.scrollTop / this.pagesEl.scrollHeight
      : 0;
    const old = this.doc;
    this.doc = doc;
    await this.layout();
    this.pagesEl.scrollTop = ratio * this.pagesEl.scrollHeight;
    void old?.loadingTask.destroy();
    this.infoEl.textContent = this.info();
  }

  info() {
    const n = this.doc?.numPages ?? 0;
    return n ? `${n} page${n === 1 ? "" : "s"}` : "";
  }

  private step(dir: number) {
    const i = ZOOMS.findIndex((z) => z >= this.scale - 0.001);
    const next = ZOOMS[Math.min(Math.max((i < 0 ? ZOOMS.length : i) + dir, 0), ZOOMS.length - 1)];
    this.setZoom(next);
  }

  private setZoom(zoom: number | null) {
    this.zoom = zoom;
    void this.layout();
  }

  /** Size every page placeholder for the current zoom; render the visible ones. */
  private async layout() {
    const doc = this.doc;
    if (!doc) return;
    const generation = ++this.generation;
    const first = await doc.getPage(1);
    const base = first.getViewport({ scale: 1 });
    const avail = this.pagesEl.clientWidth - 32;
    this.scale = this.zoom ?? (avail > 0 ? avail / base.width : 1);
    this.zoomEl.textContent = `${Math.round(this.scale * 100)}%`;

    const sizes = [];
    for (let i = 1; i <= doc.numPages; i++) {
      const page = i === 1 ? first : await doc.getPage(i);
      sizes.push(page.getViewport({ scale: this.scale }));
    }
    if (generation !== this.generation || doc !== this.doc) return;

    this.observer.disconnect();
    for (const p of this.pages) p.task?.cancel();
    this.pages = sizes.map((vp, i) => {
      const el = document.createElement("div");
      el.className = "pdf-page";
      el.dataset.page = String(i + 1);
      el.style.width = `${Math.floor(vp.width)}px`;
      el.style.height = `${Math.floor(vp.height)}px`;
      return { el, rendered: 0, task: null };
    });
    this.pagesEl.replaceChildren(...this.pages.map((p) => p.el));
    for (const p of this.pages) this.observer.observe(p.el);
  }

  private async renderPage(n: number) {
    const slot = this.pages[n - 1];
    const doc = this.doc;
    if (!slot || !doc || slot.rendered === this.scale || slot.task) return;
    const scale = this.scale;
    const page = await doc.getPage(n);
    const viewport = page.getViewport({ scale });
    const dpr = window.devicePixelRatio || 1;
    const canvas = document.createElement("canvas");
    canvas.width = Math.floor(viewport.width * dpr);
    canvas.height = Math.floor(viewport.height * dpr);
    canvas.style.width = `${Math.floor(viewport.width)}px`;
    canvas.style.height = `${Math.floor(viewport.height)}px`;
    slot.task = page.render({
      canvas,
      viewport,
      transform: dpr === 1 ? undefined : [dpr, 0, 0, dpr, 0, 0],
    });
    try {
      await slot.task.promise;
      slot.el.replaceChildren(canvas);
      slot.rendered = scale;
    } catch {
      // Cancelled by a newer zoom or reload.
    } finally {
      slot.task = null;
    }
  }

  dispose() {
    this.observer.disconnect();
    this.resize.disconnect();
    for (const p of this.pages) p.task?.cancel();
    void this.doc?.loadingTask.destroy();
  }
}
