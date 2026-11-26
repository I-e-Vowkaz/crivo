import { getCurrentWebview } from "@tauri-apps/api/webview";

// Zoom da janela com Ctrl + / Ctrl - / Ctrl 0, lembrado entre aberturas.
const KEY = "zoom";
const MIN = 0.5;
const MAX = 2;
const STEP = 0.1;

function load(): number {
  try {
    const v = Number(localStorage.getItem(KEY));
    if (v >= MIN && v <= MAX) return v;
  } catch {
    /* sem storage: fica no padrão */
  }
  return 1;
}

let zoom = load();

async function apply(next: number) {
  zoom = Math.round(Math.min(MAX, Math.max(MIN, next)) * 10) / 10;
  try {
    localStorage.setItem(KEY, String(zoom));
  } catch {
    /* ignora */
  }
  await getCurrentWebview().setZoom(zoom);
}

export function installZoom() {
  if (zoom !== 1) void apply(zoom);
  window.addEventListener("keydown", (e) => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    if (e.key === "+" || e.key === "=" || e.code === "NumpadAdd") {
      e.preventDefault();
      void apply(zoom + STEP);
    } else if (e.key === "-" || e.code === "NumpadSubtract") {
      e.preventDefault();
      void apply(zoom - STEP);
    } else if (e.key === "0" || e.code === "Numpad0") {
      e.preventDefault();
      void apply(1);
    }
  });
}
