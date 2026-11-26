// Lado a lado (padrão) ou unificado: a escolha vale para todos os diffs da tela e fica neste
// computador.
import { ref } from "vue";

const KEY = "diff.mode";

function saved(): "split" | "unified" {
  try {
    return localStorage.getItem(KEY) === "unified" ? "unified" : "split";
  } catch {
    return "split";
  }
}

export const diffMode = ref<"split" | "unified">(saved());

export function setDiffMode(m: "split" | "unified") {
  diffMode.value = m;
  try {
    localStorage.setItem(KEY, m);
  } catch {
    /* sem armazenamento: vale só nesta tela */
  }
}
