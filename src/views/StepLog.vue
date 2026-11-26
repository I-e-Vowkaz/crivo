<script setup lang="ts">
// O review como log de terminal: uma linha por etapa, com hora, duração, custo e resultado, e as
// chamadas ao Claude logo abaixo. Execuções antigas (sem etapas gravadas) mostram as etapas do
// run.log, sem hora, e as chamadas importadas dos arquivos.
import { computed } from "vue";
import type { Run, RunCall } from "../api";

const props = defineProps<{ run: Run }>();

const ms = (a: string, b: string) => new Date(b).getTime() - new Date(a).getTime();
const clock = (iso: string) => new Date(iso).toLocaleTimeString("pt-BR", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
function took(msv: number | null | undefined): string {
  if (msv === null || msv === undefined || !isFinite(msv) || msv < 0) return "";
  const s = Math.round(msv / 1000);
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${String(s % 60).padStart(2, "0")} s`;
}
const usd = (v: number) => `US$ ${v.toFixed(2)}`;
const agent: Record<string, string> = {
  "review-sonnet": "Sonnet · review",
  "quality-haiku": "Haiku · melhorias",
  "judge-sonnet": "Sonnet · juiz",
  "audit-opus": "Opus · auditoria",
  "browser-haiku": "Haiku · navegador",
};
const callText = (c: RunCall) =>
  [agent[c.name] ?? c.name, took(c.duration_ms), c.cost_usd ? usd(c.cost_usd) : "", c.turns ? `${c.turns} turno(s)` : ""]
    .filter(Boolean)
    .join(" · ");

interface Line {
  key: string;
  time: string;
  label: string;
  took: string;
  cost: string;
  state: "ok" | "run" | "fail" | "cut" | "";
  calls: RunCall[];
}

const lines = computed<Line[]>(() => {
  const run = props.run;
  if (run.steps.length) {
    const last = run.steps[run.steps.length - 1].seq;
    return run.steps.map((s) => {
      const open = s.finished_at === null;
      // a etapa em que a execução parou (falha ou cancelamento) não fica "ok"
      const stopped = s.seq === last && (run.status === "failed" || run.status === "cancelled");
      return {
        key: `s${s.seq}`,
        time: clock(s.started_at),
        label: s.label,
        took: open ? "" : took(ms(s.started_at, s.finished_at!)),
        cost: s.cost_usd ? usd(s.cost_usd) : "",
        state: s.failed || (stopped && run.status === "failed") ? "fail" : open || stopped ? (run.status === "running" ? "run" : "cut") : "ok",
        calls: run.calls.filter((c) => c.step_seq === s.seq),
      };
    });
  }
  // execução antiga: as etapas do run.log, sem hora
  return run.log
    .filter((l) => l.startsWith("== "))
    .map((l, i) => ({ key: `l${i}`, time: "", label: l.slice(3), took: "", cost: "", state: "" as const, calls: [] }));
});
const loose = computed(() => (props.run.steps.length ? [] : props.run.calls));

const total = computed(() => {
  const r = props.run;
  if (r.status === "running") return null;
  const label = r.status === "completed" ? "concluído" : r.status === "cancelled" ? "cancelado" : "falhou";
  const dur = r.finished_at ? took(ms(r.started_at, r.finished_at)) : "";
  return { label, dur, cost: r.cost_usd !== null ? usd(r.cost_usd) : "", fail: r.status !== "completed" };
});
const stateText: Record<string, string> = { ok: "ok", run: "rodando", fail: "falhou", cut: "interrompida", "": "" };
</script>

<template>
  <div class="term" role="log" aria-label="Etapas do review">
    <div class="term-bar">
      <span>{{ run.id }}</span>
      <span>{{ new Date(run.started_at).toLocaleString("pt-BR", { dateStyle: "short", timeStyle: "short" }) }}</span>
      <span v-if="!run.steps.length">execução antiga: etapas sem hora</span>
    </div>
    <ol class="term-lines">
      <li v-for="l in lines" :key="l.key" :class="['term-line', l.state]">
        <span class="term-time">{{ l.time }}</span>
        <span class="term-label">== {{ l.label }}</span>
        <span class="term-took">{{ l.took }}</span>
        <span class="term-cost">{{ l.cost }}</span>
        <span class="term-state">{{ stateText[l.state] }}</span>
        <span v-for="(c, i) in l.calls" :key="i" :class="['term-call', { fail: c.status !== 'completed' }]">
          └ {{ callText(c) }}<template v-if="c.status !== 'completed'"> · {{ c.status === "timed_out" ? "tempo esgotado" : c.status === "cancelled" ? "cancelada" : "falhou" }}</template>
        </span>
      </li>
      <li v-for="(c, i) in loose" :key="'c' + i" :class="['term-line', 'loose', { fail: c.status !== 'completed' }]">
        <span class="term-time" />
        <span class="term-label">└ {{ callText(c) }}</span>
        <span class="term-state">{{ c.status === "completed" ? "ok" : "falhou" }}</span>
      </li>
      <li v-if="total" :class="['term-line', 'term-total', { fail: total.fail }]">
        <span class="term-time">■</span>
        <span class="term-label">{{ total.label }}</span>
        <span class="term-took">{{ total.dur }}</span>
        <span class="term-cost">{{ total.cost }}</span>
      </li>
    </ol>
  </div>
</template>
