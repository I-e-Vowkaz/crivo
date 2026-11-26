<script setup lang="ts">
// Aba "Diff" do painel do review: o diff que o Claude revisou (com os achados marcados) ou a
// ponta atual da branch, todos os arquivos empilhados e carregados sob demanda.
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { api, errorText, type DiffSummary, type DiffViewKind, type Finding } from "../api";
import type { DiffMark } from "../diff";
import { diffMode, setDiffMode } from "../diffMode";
import PrDiffFile from "./PrDiffFile.vue";

const props = defineProps<{ runId: string; findings: Finding[] }>();
const emit = defineEmits<{ openFinding: [seq: number] }>();

const view = ref<DiffViewKind>("reviewed");
const summaries = ref<Record<DiffViewKind, DiffSummary | null>>({ reviewed: null, current: null });
const errors = ref<Record<DiffViewKind, string>>({ reviewed: "", current: "" });
const loading = ref(false);
const box = ref<HTMLElement | null>(null);
const index = ref<HTMLDetailsElement | null>(null);

async function load() {
  loading.value = true;
  await Promise.all(
    (["reviewed", "current"] as const).map(async (v) => {
      try {
        summaries.value[v] = await api.diffFiles(props.runId, v);
        errors.value[v] = "";
      } catch (e) {
        summaries.value[v] = null;
        errors.value[v] = errorText(e);
      }
    }),
  );
  loading.value = false;
}
onMounted(load);
watch(() => props.runId, load);

const summary = computed(() => summaries.value[view.value]);
const sha7 = (s: string | undefined) => (s ?? "").slice(0, 7);

/** As marcas só valem quando o diff mostrado é o do commit revisado. */
const marked = computed(() => view.value === "reviewed" || !!summary.value?.same_as_reviewed);

const norm = (p: string) => p.replace(/^\.?\//, "");
const sevOf = (f: Finding) => (f.kind === "melhoria" ? "melhoria" : ((f.audit_new_severity as Finding["severity"]) ?? f.severity ?? "low"));

/** arquivo → linha do lado novo → selos. Ficam de fora os rejeitados e os falsos positivos. */
const marksByFile = computed(() => {
  const out: Record<string, Record<number, DiffMark[]>> = {};
  if (!marked.value) return out;
  for (const f of props.findings) {
    if (f.validation !== "ok" || f.audit_decision === "falso_positivo" || !f.file || !f.line) continue;
    const sev = sevOf(f);
    const byLine = (out[norm(f.file)] ??= {});
    (byLine[f.line] ??= []).push({ seq: f.seq, label: sev, cls: sev, title: f.title ?? "" });
  }
  return out;
});
const NONE: Record<number, DiffMark[]> = {};
const marksOf = (path: string) => marksByFile.value[path] ?? NONE;
const countOf = (path: string) => Object.values(marksOf(path)).reduce((n, ms) => n + ms.length, 0);

const statusLabel = { added: "novo", deleted: "apagado", renamed: "renomeado", modified: "" } as const;

const totals = computed(() =>
  (summary.value?.files ?? []).reduce((t, f) => ({ added: t.added + f.added, removed: t.removed + f.removed }), { added: 0, removed: 0 }),
);

/** Fecha a lista antes de rolar: aberta, ela empurraria o arquivo para baixo. */
async function jump(i: number) {
  if (index.value) index.value.open = false;
  await nextTick();
  box.value?.querySelector(`#pr-diff-file-${i}`)?.scrollIntoView({ block: "start" });
}
</script>

<template>
  <div ref="box" class="panel-scroll pr-diff">
    <div class="pr-diff-bar">
      <nav class="mode-switch">
        <button :class="['small-btn', view === 'reviewed' ? 'on' : 'secondary']" @click="view = 'reviewed'">
          Revisado<template v-if="summaries.reviewed"> · {{ sha7(summaries.reviewed.reviewed_sha) }}</template>
        </button>
        <button :class="['small-btn', view === 'current' ? 'on' : 'secondary']" @click="view = 'current'">
          Atual<template v-if="summaries.current?.head_sha"> · {{ sha7(summaries.current.head_sha) }}</template>
        </button>
      </nav>
      <span v-if="summaries.current && !summaries.current.missing" class="muted small">
        <template v-if="summaries.current.same_as_reviewed">atual igual ao revisado</template>
        <template v-else-if="summaries.current.ahead !== null">+{{ summaries.current.ahead }} commit(s) depois do review</template>
        <template v-else>a branch mudou depois do review</template>
      </span>
      <span class="diff-mode" role="group" aria-label="Modo do diff">
        <button type="button" :class="['icon-btn', { on: diffMode === 'split' }]" :aria-pressed="diffMode === 'split'" @click="setDiffMode('split')">
          lado a lado
        </button>
        <button type="button" :class="['icon-btn', { on: diffMode === 'unified' }]" :aria-pressed="diffMode === 'unified'" @click="setDiffMode('unified')">
          unificado
        </button>
      </span>
    </div>

    <p v-if="loading && !summary" class="muted small"><span class="spinner" /> lendo o diff…</p>
    <p v-else-if="errors[view]" class="msg error">{{ errors[view] }}</p>
    <p v-else-if="summary?.missing" class="msg warn">{{ summary.missing }}</p>
    <template v-else-if="summary">
      <p v-if="!marked" class="muted small">Os achados não são marcados aqui: a ponta atual não é o commit revisado.</p>
      <p v-if="!summary.files.length" class="muted">Nenhum arquivo alterado.</p>
      <details v-else ref="index" class="pr-diff-files">
        <summary>{{ summary.files.length }} arquivo(s) · +{{ totals.added }} −{{ totals.removed }}</summary>
        <ul class="pr-diff-index">
          <li v-for="(f, i) in summary.files" :key="f.path">
            <button type="button" class="icon-btn" @click="jump(i)">
              <code>{{ f.old_path ? `${f.old_path} → ${f.path}` : f.path }}</code>
            </button>
            <span v-if="statusLabel[f.status]" class="badge">{{ statusLabel[f.status] }}</span>
            <span class="muted small">+{{ f.added }} −{{ f.removed }}</span>
            <span v-if="countOf(f.path)" class="muted small">· {{ countOf(f.path) }} achado(s)</span>
          </li>
        </ul>
      </details>
      <PrDiffFile
        v-for="(f, i) in summary.files"
        :id="`pr-diff-file-${i}`"
        :key="`${view}:${f.path}`"
        :run-id="runId"
        :view="view"
        :file="f"
        :base-sha="summary.base_sha"
        :head-sha="summary.head_sha"
        :marks="marksOf(f.path)"
        @mark="(s) => emit('openFinding', s)"
      />
    </template>
  </div>
</template>
