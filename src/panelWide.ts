// Painel do review (e da correção) na largura toda, com a lista escondida. Vale para os painéis
// abertos nesta sessão, inclusive ao trocar entre Review e Correções numa branch.
import { ref } from "vue";

export const panelWide = ref(false);

export function togglePanelWide() {
  panelWide.value = !panelWide.value;
}
