<script setup lang="ts">
// Um arquivo do diff do PR: carregado só quando está aberto e chega perto da tela, uma vez por
// visão. Lock files, gerados, binários e arquivos grandes começam fechados.
import { computed, onMounted, onUnmounted, ref } from "vue";
import { api, errorText, type DiffFile, type DiffViewKind, type FileDiff } from "../api";
import { hunkRows, type DiffMark } from "../diff";
import DiffRows from "./DiffRows.vue";

const props = defineProps<{
  runId: string;
  view: DiffViewKind;
  file: DiffFile;
  /** Commits do resumo mostrado: os arquivos não mudam se um sync rodar com a aba aberta. */
  baseSha: string;
  headSha: string;
  /** Selos dos achados deste arquivo, por linha do lado novo. */
  marks: Record<number, DiffMark[]>;
}>();
const emit = defineEmits<{ mark: [seq: number] }>();

const GENERATED = /(^|\/)(package-lock\.json|composer\.lock|yarn\.lock|pnpm-lock\.yaml)$|\.min\.(js|css)$|(^|\/)(dist|build)\//;
const MAX_LINES = 1500;

const closedReason = computed(() => {
  if (props.file.binary) return "binário";
  if (GENERATED.test(props.file.path)) return "gerado";
  if (props.file.added + props.file.removed > MAX_LINES) return "grande";
  return "";
});

const open = ref(!closedReason.value);
const near = ref(false);
const loading = ref(false);
const error = ref("");
const diff = ref<FileDiff | null>(null);
const root = ref<HTMLElement | null>(null);
let observer: IntersectionObserver | null = null;

async function load() {
  if (diff.value || loading.value || props.file.binary) return;
  loading.value = true;
  error.value = "";
  try {
    diff.value = await api.diffFile(props.runId, props.view, props.baseSha, props.headSha, props.file.path, props.file.old_path);
  } catch (e) {
    error.value = errorText(e);
  } finally {
    loading.value = false;
  }
}

function toggle() {
  open.value = !open.value;
  if (open.value) load();
}

onMounted(() => {
  observer = new IntersectionObserver(
    (entries) => {
      near.value = entries.some((e) => e.isIntersecting);
      if (near.value && open.value) load();
    },
    { rootMargin: "800px" },
  );
  if (root.value) observer.observe(root.value);
});
onUnmounted(() => observer?.disconnect());

const rows = computed(() => (diff.value ? hunkRows(diff.value.hunks) : []));

/** Achados cuja linha não aparece em nenhum trecho: ficam no cabeçalho, para não sumirem. */
const outside = computed(() => {
  const lines = Object.keys(props.marks).map(Number);
  // fechado ou ainda sem carregar: o cabeçalho só mostra a contagem
  if (!lines.length || !open.value || (!diff.value && !props.file.binary)) return [];
  const shown = new Set<number>();
  for (const h of diff.value?.hunks ?? []) for (const l of h.lines) if (l.new_no !== null) shown.add(l.new_no);
  return lines.filter((n) => !shown.has(n)).flatMap((n) => props.marks[n].map((m) => ({ ...m, line: n })));
});

const markCount = computed(() => Object.values(props.marks).reduce((n, ms) => n + ms.length, 0));

const statusLabel: Record<DiffFile["status"], string> = { added: "novo", deleted: "apagado", renamed: "renomeado", modified: "" };
</script>

<template>
  <section ref="root" class="diff pr-diff-file">
    <div class="diff-head">
      <button type="button" class="icon-btn" :aria-expanded="open" @click="toggle">{{ open ? "▾" : "▸" }}</button>
      <code v-if="file.old_path">{{ file.old_path }} → {{ file.path }}</code>
      <code v-else>{{ file.path }}</code>
      <span v-if="statusLabel[file.status]" class="badge">{{ statusLabel[file.status] }}</span>
      <span v-if="!open && closedReason" class="badge">{{ closedReason }}</span>
      <span v-if="markCount" class="muted small">{{ markCount }} achado(s)</span>
      <span class="muted small file-stat">+{{ file.added }} −{{ file.removed }}</span>
    </div>
    <div v-if="outside.length" class="outside-marks">
      <span v-for="m in outside" :key="`${m.seq}-${m.line}`" class="outside-mark">
        <button type="button" :class="['sev', 'diff-mark', m.cls]" :title="m.title" @click="emit('mark', m.seq)">{{ m.label }}</button>
        <span class="muted small">linha {{ m.line }} fora dos trechos</span>
      </span>
    </div>
    <template v-if="open">
      <p v-if="file.binary" class="muted small pr-diff-note">arquivo binário</p>
      <p v-else-if="error" class="msg error pr-diff-note">{{ error }}</p>
      <p v-else-if="loading || !diff" class="muted small pr-diff-note"><span class="spinner" /> carregando…</p>
      <template v-else>
        <p v-if="!diff.hunks.length" class="muted small pr-diff-note">sem mudança de conteúdo</p>
        <DiffRows v-else :rows="rows" :marks="marks" left-label="base" right-label="PR" @mark="(s) => emit('mark', s)" />
        <p v-if="diff.truncated" class="msg warn pr-diff-note">Diff cortado em 1 MB.</p>
      </template>
    </template>
  </section>
</template>
