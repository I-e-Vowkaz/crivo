<script setup lang="ts">
// Custo e tempo dos reviews: média por review, custo de cada agente por chamada e os últimos
// 30 reviews concluídos. Lê `usage_get`, que importa antes as chamadas das execuções antigas.
import { computed, onMounted, ref } from "vue";
import { api, errorText, formatDate, type Usage } from "../api";

const usage = ref<Usage | null>(null);
const error = ref("");
const loading = ref(false);

async function load() {
  loading.value = true;
  error.value = "";
  try {
    usage.value = await api.usageGet();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

const usd = (v: number, d = 2) => `US$ ${v.toFixed(d)}`;
const secs = (msv: number | null) => (msv === null ? "—" : msv < 60_000 ? `${Math.round(msv / 1000)} s` : `${(msv / 60_000).toFixed(1)} min`);
const agentLabel: Record<string, string> = {
  "review-sonnet": "Sonnet · review",
  "audit-opus": "Opus · auditoria",
  "judge-sonnet": "Sonnet · juiz",
  "quality-haiku": "Haiku · melhorias",
  "browser-haiku": "Haiku · navegador",
};

// ---- custo médio por chamada, por agente (barras horizontais) ----
const BAR = { left: 260, width: 330, row: 40 };
const agentMax = computed(() => Math.max(0.0001, ...(usage.value?.agents ?? []).map((a) => a.cost_avg)));
/** Escala com marcas redondas: 0,05 ou 0,10 de passo, até cobrir a maior média. */
const agentTicks = computed(() => {
  const step = agentMax.value > 0.2 ? 0.1 : 0.05;
  const top = Math.ceil(agentMax.value / step) * step;
  return { top, ticks: Array.from({ length: Math.round(top / step) + 1 }, (_, i) => +(i * step).toFixed(2)) };
});
const ax = (v: number) => BAR.left + (v / agentTicks.value.top) * BAR.width;

// ---- custo dos últimos reviews (colunas) ----
const COL = { left: 44, top: 12, width: 560, height: 150 };
const runMax = computed(() => {
  const m = Math.max(0.0001, ...(usage.value?.recent ?? []).map((r) => r.cost_usd));
  const step = m > 0.6 ? 0.25 : 0.1;
  const top = Math.ceil(m / step) * step;
  return { top, ticks: Array.from({ length: Math.round(top / step) + 1 }, (_, i) => +(i * step).toFixed(2)) };
});
const ry = (v: number) => COL.top + COL.height - (v / runMax.value.top) * COL.height;
const colW = computed(() => COL.width / Math.max(1, usage.value?.recent.length ?? 1));
</script>

<template>
  <div class="usage">
    <div class="toolbar">
      <button class="secondary" :disabled="loading" @click="load">{{ loading ? "Lendo…" : "Recarregar" }}</button>
      <span class="muted small">custos do próprio Claude Code (<code>total_cost_usd</code> de cada chamada)</span>
    </div>
    <p v-if="error" class="msg error">{{ error }}</p>

    <template v-if="usage">
      <section class="card">
        <h2>Custo por review</h2>
        <div class="kpis">
          <div class="kpi"><span class="tile-label">Média por review</span><b class="on">{{ usd(usage.cost_avg) }}</b></div>
          <div class="kpi"><span class="tile-label">Faixa</span><b>{{ usage.cost_min.toFixed(2) }} – {{ usage.cost_max.toFixed(2) }}</b></div>
          <div class="kpi"><span class="tile-label">Reviews concluídos</span><b>{{ usage.runs_completed }}</b></div>
          <div class="kpi"><span class="tile-label">Total gasto</span><b>{{ usd(usage.cost_total) }}</b></div>
        </div>

        <h3>Últimos {{ usage.recent.length }} reviews</h3>
        <p v-if="!usage.recent.length" class="muted small">Nenhum review concluído ainda.</p>
        <div v-else class="chart-wrap">
          <svg
            class="chart"
            :style="{ maxWidth: `${COL.left + COL.width + 80}px` }"
            :viewBox="`0 0 ${COL.left + COL.width + 80} ${COL.top + COL.height + 26}`"
            role="img"
            :aria-label="`Custo dos últimos ${usage.recent.length} reviews; média ${usd(usage.cost_avg)}`"
          >
            <g v-for="t in runMax.ticks" :key="t">
              <line class="grid-line" :x1="COL.left" :x2="COL.left + COL.width" :y1="ry(t)" :y2="ry(t)" />
              <text class="axis" :x="COL.left - 6" :y="ry(t) + 4" text-anchor="end">{{ t.toFixed(2) }}</text>
            </g>
            <rect
              v-for="(r, i) in usage.recent"
              :key="r.id"
              :class="['col', { high: r.cost_usd > usage.cost_avg * 1.6 }]"
              :x="COL.left + i * colW + colW * 0.18"
              :y="ry(r.cost_usd)"
              :width="colW * 0.64"
              :height="COL.top + COL.height - ry(r.cost_usd)"
            >
              <title>{{ r.label }} · {{ formatDate(r.started_at) }} · {{ usd(r.cost_usd) }}</title>
            </rect>
            <line class="avg-line" :x1="COL.left" :x2="COL.left + COL.width" :y1="ry(usage.cost_avg)" :y2="ry(usage.cost_avg)" />
            <text class="axis on" :x="COL.left + COL.width + 6" :y="ry(usage.cost_avg) + 4">média {{ usage.cost_avg.toFixed(2) }}</text>
            <text class="axis" :x="COL.left" :y="COL.top + COL.height + 18">{{ formatDate(usage.recent[0].started_at) }}</text>
            <text class="axis" :x="COL.left + COL.width" :y="COL.top + COL.height + 18" text-anchor="end">
              {{ formatDate(usage.recent[usage.recent.length - 1].started_at) }}
            </text>
          </svg>
        </div>
        <p class="muted small">Em laranja, o review que custou mais de 1,6 vez a média (em geral, quando o Opus entra com muitos achados).</p>
      </section>

      <section class="card">
        <h2>Custo por agente</h2>
        <p class="muted small">Custo médio de cada chamada, com o número de chamadas e a duração média.</p>
        <p v-if="!usage.agents.length" class="muted small">Nenhuma chamada registrada ainda.</p>
        <div v-else class="chart-wrap">
          <svg
            class="chart"
            :style="{ maxWidth: `${BAR.left + BAR.width + 70}px` }"
            :viewBox="`0 0 ${BAR.left + BAR.width + 70} ${usage.agents.length * BAR.row + 28}`"
            role="img"
            :aria-label="usage.agents.map((a) => `${agentLabel[a.name] ?? a.name}: ${usd(a.cost_avg, 3)} por chamada`).join('; ')"
          >
            <g v-for="t in agentTicks.ticks" :key="t">
              <line class="grid-line" :x1="ax(t)" :x2="ax(t)" y1="4" :y2="usage.agents.length * BAR.row" />
              <text class="axis" :x="ax(t)" :y="usage.agents.length * BAR.row + 18" text-anchor="middle">{{ t.toFixed(2) }}</text>
            </g>
            <g v-for="(a, i) in usage.agents" :key="a.name">
              <text class="label" x="0" :y="i * BAR.row + 18">{{ agentLabel[a.name] ?? a.name }}</text>
              <text :class="['axis', { al: a.failed }]" x="0" :y="i * BAR.row + 32">
                {{ a.calls }} chamada(s) · {{ secs(a.duration_avg_ms) }}<template v-if="a.failed"> · {{ a.failed }} falha(s)</template>
              </text>
              <rect class="bar" :x="BAR.left" :y="i * BAR.row + 8" :width="Math.max(2, ax(a.cost_avg) - BAR.left)" height="18" />
              <text class="value" :x="ax(a.cost_avg) + 6" :y="i * BAR.row + 22">{{ a.cost_avg.toFixed(3) }}</text>
            </g>
          </svg>
        </div>
        <table class="usage-table">
          <thead>
            <tr><th>Agente</th><th>Modelo</th><th>Chamadas</th><th>Falhas</th><th>Média</th><th>Total</th></tr>
          </thead>
          <tbody>
            <tr v-for="a in usage.agents" :key="a.name">
              <td>{{ agentLabel[a.name] ?? a.name }}</td>
              <td>{{ a.model ?? "—" }}</td>
              <td>{{ a.calls }}</td>
              <td :class="{ alert: a.failed }">{{ a.failed }}</td>
              <td>{{ usd(a.cost_avg, 3) }}</td>
              <td>{{ usd(a.cost_total) }}</td>
            </tr>
          </tbody>
        </table>
      </section>
    </template>
  </div>
</template>
