// A small modal dialog. The webview's own alert/confirm are not reliable
// inside Tauri, so this replaces them.

const dialog = document.getElementById("modal") as HTMLDialogElement;
const textEl = document.getElementById("modal-text")!;
const detailEl = document.getElementById("modal-detail")!;
const buttonsEl = document.getElementById("modal-buttons")!;

export interface Choice {
  value: string;
  label: string;
  primary?: boolean;
  danger?: boolean;
}

// One dialog at a time: later questions wait for earlier ones to be answered.
let queue: Promise<unknown> = Promise.resolve();

/**
 * Show `text` with one button per choice and resolve to the chosen value.
 * Escape resolves to "cancel".
 */
export function ask(text: string, choices: Choice[], detail?: string): Promise<string> {
  const run = () => show(text, choices, detail);
  const answer = queue.then(run, run);
  queue = answer.catch(() => {});
  return answer;
}

function show(text: string, choices: Choice[], detail?: string): Promise<string> {
  textEl.textContent = text;
  detailEl.hidden = !detail;
  detailEl.textContent = detail ?? "";
  buttonsEl.replaceChildren(
    ...choices.map((c) => {
      const b = document.createElement("button");
      b.value = c.value;
      b.textContent = c.label;
      if (c.primary) b.classList.add("primary");
      if (c.danger) b.classList.add("danger");
      return b;
    }),
  );
  dialog.returnValue = "cancel";
  dialog.showModal();
  (buttonsEl.querySelector(".primary") as HTMLButtonElement | null)?.focus();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue || "cancel"), { once: true });
  });
}

export const tell = (text: string, detail?: string) =>
  ask(text, [{ value: "ok", label: "OK", primary: true }], detail);
