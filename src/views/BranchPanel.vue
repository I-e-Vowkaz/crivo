<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { api, errorText, type FixComment, type LocalBranch } from "../api";
import FixPanel from "./FixPanel.vue";
import ReviewPanel from "./ReviewPanel.vue";

/** Painel de uma branch local: o review (achados e melhorias) e as correções, no mesmo lugar. */
const props = defineProps<{ slug: string; branch: LocalBranch }>();
const emit = defineEmits<{ close: []; changed: [] }>();

const mode = ref<"review" | "fix">(props.branch.last_run || !props.branch.fix ? "review" : "fix");
const runId = ref<string | null>(props.branch.last_run?.id ?? null);
const error = ref("");
const starting = ref(false);

// um review novo (aberto pela lista) troca o painel para ele
watch(
  () => props.branch.last_run?.id,
  (id) => {
    if (id && id !== runId.value) runId.value = id;
  },
);

/** Situação, na correção, de cada achado deste review. */
const fixState = computed(() => {
  const out: Record<number, FixComment["status"]> = {};
  for (const [run, seq, status] of props.branch.fix?.findings ?? []) if (run === runId.value) out[seq] = status;
  return out;
});
const fixCount = computed(() => props.branch.fix?.total ?? 0);

async function review() {
  error.value = "";
  starting.value = true;
  try {
    runId.value = await api.reviewStartBranch(props.slug, props.branch.name);
    mode.value = "review";
    emit("changed");
  } catch (e) {
    error.value = errorText(e);
  } finally {
    starting.value = false;
  }
}

async function fix(seqs: number[]) {
  if (!runId.value) return;
  error.value = "";
  try {
    await api.fixAddFindings(runId.value, seqs);
    mode.value = "fix";
    emit("changed");
  } catch (e) {
    error.value = errorText(e);
  }
}
</script>

<template>
  <ReviewPanel
    v-if="mode === 'review' && runId"
    :key="runId"
    :heading="`Review ${branch.name}`"
    :subtitle="`${slug} · comparada com a base da origin`"
    :run-id="runId"
    fixable
    :fix-state="fixState"
    @close="emit('close')"
    @rerun="(id) => { runId = id; emit('changed'); }"
    @fix="fix"
  >
    <template #switch>
      <nav class="mode-switch">
        <button class="small-btn on">Review</button>
        <button class="small-btn secondary" @click="mode = 'fix'">Correções ({{ fixCount }})</button>
      </nav>
      <p v-if="error" class="msg error panel-error">{{ error }}</p>
    </template>
  </ReviewPanel>

  <FixPanel v-else-if="mode === 'fix'" :key="`fix-${slug}-${branch.name}`" :branch="{ slug, name: branch.name }" @close="emit('close')">
    <template #switch>
      <nav class="mode-switch">
        <button class="small-btn secondary" @click="mode = 'review'">Review</button>
        <button class="small-btn on">Correções ({{ fixCount }})</button>
      </nav>
    </template>
  </FixPanel>

  <aside v-else class="panel">
    <header class="panel-head">
      <div>
        <h2>{{ branch.name }}</h2>
        <div class="muted small">{{ slug }}</div>
      </div>
      <button class="secondary" @click="emit('close')">Fechar</button>
    </header>
    <div class="panel-scroll">
      <p v-if="error" class="msg error">{{ error }}</p>
      <p class="muted">
        Esta branch ainda não foi revisada. O review compara o que está commitado nela com a base da origin, com o mesmo
        fluxo de "Para eu revisar" (Sonnet, melhorias do Haiku e auditoria do Opus), sem o teste no navegador. Depois,
        cada achado pode ser corrigido daqui.
      </p>
      <button :disabled="starting" @click="review">{{ starting ? "Iniciando…" : "Revisar" }}</button>
    </div>
  </aside>
</template>
