<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { openUrl } from "@tauri-apps/plugin-opener";
import { marked } from "marked";
import DOMPurify from "dompurify";
import DiffView from "./DiffView.vue";
import PipelineView from "./PipelineView.vue";
import PrDiff from "./PrDiff.vue";
import StepLog from "./StepLog.vue";
import { panelWide, togglePanelWide } from "../panelWide";
import { api, errorText, events, formatDate, type ChatMessage, type Finding, type FixComment, type Pr, type Run } from "../api";

const props = defineProps<{
  /** PR revisado; sem ele (branch local), o cabeçalho usa `heading` e `subtitle`. */
  pr?: Pr;
  heading?: string;
  subtitle?: string;
  runId: string;
  /** Branch local: cada achado e melhoria ganha "Corrigir". */
  fixable?: boolean;
  /** Situação na correção de cada achado já enviado (seq → status do item). */
  fixState?: Record<number, FixComment["status"]>;
}>();
const emit = defineEmits<{ close: []; rerun: [runId: string]; fix: [seqs: number[]] }>();

const fixLabel: Record<FixComment["status"], string> = {
  new: "não analisado",
  queued: "na fila da correção",
  analyzing: "corrigindo",
  procede: "sugestão pronta",
  nao_procede: "não procede",
  resposta: "respondido",
  applied: "corrigido",
  failed: "correção falhou",
};

const run = ref<Run | null>(null);
const error = ref("");
const logBox = ref<HTMLElement | null>(null);
const chatScroll = ref<HTMLElement | null>(null);
/** Aba do painel: o review, a conversa ou o diff; as duas últimas ocupam o painel inteiro. */
const view = ref<"review" | "chat" | "diff">("review");
/** A aba Diff só monta (e lê o git) na primeira vez que é aberta; depois fica guardada. */
const diffOpened = ref(false);
const flashSeq = ref<number | null>(null);
const showRejected = ref(false);
const selectedForPublish = ref<number[]>([]);
const publishing = ref(false);
const publishMessage = ref("");
const publishedHere = ref<number[]>([]);
/** Publicar só onde o Crivo escreve: no GitHub. O Bitbucket é só leitura. */
const canPublish = computed(() => props.pr?.provider === "github" && run.value?.status === "completed");
const selectedText = computed(() => main.value.filter((f) => selectedForPublish.value.includes(f.seq)).map(findingText).join("\n\n---\n\n"));

function togglePublish(seq: number) {
  selectedForPublish.value = selectedForPublish.value.includes(seq)
    ? selectedForPublish.value.filter((s) => s !== seq)
    : [...selectedForPublish.value, seq];
}

async function publish(requestChanges: boolean) {
  if (!selectedForPublish.value.length) return;
  if (!window.confirm(`Publicar ${selectedForPublish.value.length} achado(s) na PR?`)) return;
  publishing.value = true;
  publishMessage.value = "";
  try {
    const sent = await api.publishFindings(props.runId, selectedForPublish.value, requestChanges);
    publishedHere.value.push(...sent.map((s) => s.seq));
    selectedForPublish.value = [];
    publishMessage.value = `${sent.length} achado(s) publicado(s).`;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    publishing.value = false;
  }
}
// ---- pedido de mudanças (rc-validator) ----
/** Itens que entram no RC: bloqueantes primeiro. */
const rcItems = computed(() =>
  (run.value?.findings ?? [])
    .filter((f) => (f.rc_decision === "bloqueante" || f.rc_decision === "sugestao") && f.rc_body)
    .sort((a, b) => (a.rc_decision === b.rc_decision ? a.seq - b.seq : a.rc_decision === "bloqueante" ? -1 : 1)),
);
const rcOut = computed(() => (run.value?.findings ?? []).filter((f) => f.rc_decision === "fora"));
const rcDuplicates = (seq: number) => rcOut.value.filter((f) => f.rc_duplicate_of === seq).map((f) => `F${f.seq}`);
/** Itens que o revisor tirou antes de enviar. */
const rcSkip = ref<number[]>([]);
const rcSelected = computed(() => rcItems.value.filter((f) => !rcSkip.value.includes(f.seq)));
const rcBlocking = computed(() => rcSelected.value.some((f) => f.rc_decision === "bloqueante"));
const sendingRc = ref(false);

function toggleRcSkip(seq: number) {
  rcSkip.value = rcSkip.value.includes(seq) ? rcSkip.value.filter((s) => s !== seq) : [...rcSkip.value, seq];
}

async function sendRc() {
  const n = rcSelected.value.length;
  if (!n) return;
  const what = rcBlocking.value ? "Pedir mudanças" : "Comentar";
  if (!window.confirm(`${what} na PR com ${n} comentário(s)?`)) return;
  sendingRc.value = true;
  error.value = "";
  try {
    await api.sendRequestChanges(props.runId, rcSkip.value);
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    sendingRc.value = false;
  }
}

const unlisten: (() => void)[] = [];

async function load() {
  try {
    run.value = await api.reviewGet(props.runId);
    error.value = "";
    scrollLog();
  } catch (e) {
    error.value = errorText(e);
  }
}

function scrollLog() {
  nextTick(() => {
    if (logBox.value) logBox.value.scrollTop = logBox.value.scrollHeight;
  });
}

async function cancel() {
  try {
    await api.reviewCancel(props.runId);
  } catch (e) {
    error.value = errorText(e);
  }
}

const sevOrder = { critical: 0, high: 1, medium: 2, low: 3 } as const;
const effectiveSeverity = (f: Finding) => (f.audit_new_severity as Finding["severity"]) ?? f.severity;

const main = computed(() =>
  (run.value?.findings ?? [])
    .filter((f) => f.kind !== "melhoria" && f.validation === "ok" && f.audit_decision !== "falso_positivo")
    .sort((a, b) => sevOrder[effectiveSeverity(a) ?? "low"] - sevOrder[effectiveSeverity(b) ?? "low"]),
);
/** Melhorias técnicas (complexidade desnecessária, desempenho e testes), do Haiku: não são bugs e não têm severidade. */
/** Pela nota do juiz, da maior para a menor; sem nota (juiz falhou) no fim. */
const improvements = computed(() =>
  (run.value?.findings ?? [])
    .filter((f) => f.kind === "melhoria" && f.validation === "ok")
    .sort((a, b) => (b.score ?? -1) - (a.score ?? -1)),
);
const importanceLabel: Record<string, string> = { alta: "alta", media: "média", baixa: "baixa" };
const lineRange = (f: Finding) => (f.end_line && f.end_line > (f.line ?? 0) ? `${f.line}-${f.end_line}` : `${f.line}`);
const categoryLabel: Record<string, string> = {
  redundante: "redundante",
  codigo_morto: "código morto",
  simplificavel: "simplificável",
  duplicado: "duplicado",
  abstracao_desnecessaria: "abstração desnecessária",
  desempenho: "desempenho",
  teste: "teste",
};

/** Falsos positivos do Opus: ficam na lista, riscados e com o carimbo, em vez de sumir. */
const falsePositives = computed(() =>
  (run.value?.findings ?? []).filter((f) => f.kind !== "melhoria" && f.validation === "ok" && f.audit_decision === "falso_positivo"),
);
const discarded = computed(() => (run.value?.findings ?? []).filter((f) => f.validation === "rejected"));

/** Carimbo do veredito do Opus. */
function stamp(f: Finding): { text: string; tone: string } | null {
  switch (f.audit_decision) {
    case "confirmado":
      return { text: "Confirmado", tone: "ok" };
    case "severidade_ajustada":
      return { text: `Ajustado · era ${f.severity ?? "?"}`, tone: "mid" };
    case "evidencia_insuficiente":
      return { text: "Evidência insuficiente", tone: "al" };
    case "falso_positivo":
      return { text: "Falso positivo", tone: "al" };
    default:
      return null;
  }
}
/** Sugestão da melhoria como diff (sem código novo numa de desempenho ou teste, a correção está na descrição). */
const diffOf = (f: Finding) =>
  f.existing_code === null || ((f.category === "desempenho" || f.category === "teste") && !f.improved_code)
    ? null
    : { file: f.file ?? "", line: f.line ?? 0, end_line: f.end_line ?? f.line ?? 0, existing_code: f.existing_code, improved_code: f.improved_code ?? "" };

const copied = ref("");

// ---- chat ----
const chat = ref<ChatMessage[]>([]);
const chatBusy = ref(false);
const chatProgress = ref<string[]>([]);
const question = ref("");
const input = ref<HTMLTextAreaElement | null>(null);

/** Resposta do Claude em Markdown → HTML sanitizado (a resposta pode citar trechos do repositório). */
const renderMd = (text: string) => DOMPurify.sanitize(marked.parse(text, { async: false, gfm: true, breaks: true }) as string);

function scrollToBottom(smooth = true) {
  nextTick(() => chatScroll.value?.scrollTo({ top: chatScroll.value.scrollHeight, behavior: smooth ? "smooth" : "auto" }));
}

/** Clique em qualquer parte da caixa de pergunta (fora dos botões) põe o cursor no campo. */
function focusInput(e: MouseEvent) {
  if (!(e.target as HTMLElement).closest("button, textarea")) input.value?.focus();
}

function openChat() {
  view.value = "chat";
  scrollToBottom(false);
  nextTick(() => input.value?.focus());
}

function openDiff() {
  view.value = "diff";
  diffOpened.value = true;
}

/** Selo de um achado no diff → volta para a aba Review e mostra o achado. */
async function goFinding(seq: number) {
  view.value = "review";
  await nextTick();
  document.getElementById(`finding-${seq}`)?.scrollIntoView({ block: "center" });
  flashSeq.value = seq;
  setTimeout(() => {
    if (flashSeq.value === seq) flashSeq.value = null;
  }, 1500);
}

async function loadChat(scroll = false) {
  try {
    const r = await api.chatGet(props.runId);
    chat.value = r.messages;
    chatBusy.value = r.busy;
    if (!r.busy) chatProgress.value = [];
    if (scroll) scrollToBottom();
  } catch (e) {
    error.value = errorText(e);
  }
}

function autosize() {
  const el = input.value;
  if (!el) return;
  const max = Math.round(window.innerHeight * 0.4);
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, max)}px`;
  // rola só quando o texto passa da altura máxima (folga de 2 px para o arredondamento do zoom)
  el.style.overflowY = el.value && el.scrollHeight > max + 2 ? "auto" : "hidden";
}

async function ask(text?: string) {
  const q = (text ?? question.value).trim();
  if (!q || chatBusy.value) return;
  error.value = "";
  try {
    await api.chatAsk(props.runId, q);
    question.value = "";
    chatProgress.value = [];
    nextTick(autosize);
    await loadChat(true);
  } catch (e) {
    error.value = errorText(e);
  }
}

async function stopChat() {
  try {
    await api.chatCancel(props.runId);
  } catch (e) {
    error.value = errorText(e);
  }
}

const suggestions = computed(() => {
  const s = main.value.slice(0, 2).map((f) => `O achado F${f.seq} procede no código atual?`);
  s.push("Procure outros bugs desta task no diff");
  return s;
});

const checkoutBusy = ref(false);
const checkoutDone = ref("");

/** Checkout do PR indicado no alerta do navegador (o próprio ou o do outro lado do card). */
async function checkoutFromAlert() {
  if (!run.value?.browser_checkout_pr_id) return;
  checkoutBusy.value = true;
  error.value = "";
  try {
    const r = await api.checkoutPr(run.value.browser_checkout_pr_id);
    checkoutDone.value = r.at_pr_commit
      ? `${r.repo} agora está em ${r.branch} (${r.head.slice(0, 10)}).`
      : `${r.repo} está em ${r.branch} (${r.head.slice(0, 10)}), mas não no commit que o app conhece; clique em Atualizar na lista.`;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    checkoutBusy.value = false;
  }
}

async function rerun() {
  if (!run.value) return;
  try {
    const r = run.value;
    emit("rerun", r.pr_id !== null ? await api.reviewStart(r.pr_id) : await api.reviewStartBranch(r.repo_slug ?? "", r.branch ?? ""));
  } catch (e) {
    error.value = errorText(e);
  }
}

/** Texto pronto para colar (ex.: comentário no PR): o bloco no formato do branch-reviewer. */
function findingText(f: Finding) {
  if (f.markdown?.trim()) return f.markdown.trim();
  const steps = f.steps.map((s, i) => `${i + 1}. ${s}`).join("\n");
  return [`\`${f.file}:${f.line}\` — ${f.title}`, f.description, steps, f.pattern_ref ? `Padrão do projeto: ${f.pattern_ref}` : ""]
    .filter(Boolean)
    .join("\n\n");
}

async function copy(text: string, what: string) {
  try {
    await writeText(text);
    copied.value = what;
    setTimeout(() => (copied.value = ""), 2000);
  } catch (e) {
    error.value = errorText(e);
  }
}

/** Achados ainda fora da correção (ou cuja correção falhou). */
const toFix = (list: Finding[]) => list.filter((f) => !props.fixState?.[f.seq] || props.fixState[f.seq] === "failed").map((f) => f.seq);

const copyAll = () => copy(main.value.map(findingText).join("\n\n---\n\n"), "todos");
const copyAllImprovements = () => copy(improvements.value.map(findingText).join("\n\n---\n\n"), "melhorias");

const testLabel: Record<string, string> = {
  aprovado: "aprovado",
  reprovado: "reprovado",
  bloqueado: "bloqueado",
  nao_executado: "não executado",
};
const browserLabel: Record<string, string> = {
  blocked: "não rodou",
  running: "rodando",
  completed: "concluído",
  failed: "falhou",
  cancelled: "cancelado",
};

onMounted(async () => {
  await load();
  await loadChat();
  // pergunta ainda em andamento: reabre direto na conversa (só se a aba existe; sem o run, o painel ficaria vazio)
  if (chatBusy.value && run.value && run.value.status !== "running") openChat();
  unlisten.push(
    await events.onChatUpdate((e) => {
      if (e.run_id !== props.runId) return;
      if (e.line) {
        chatBusy.value = true;
        chatProgress.value = [...chatProgress.value, e.line];
        scrollToBottom();
      } else {
        loadChat(true);
      }
    }),
  );
  unlisten.push(
    await events.onReviewLog((e) => {
      if (e.run_id === props.runId && run.value) {
        run.value.log.push(e.line);
        scrollLog();
      }
    }),
  );
  unlisten.push(
    await events.onReviewUpdate((e) => {
      if (e.run_id === props.runId) load();
    }),
  );
});
onUnmounted(() => unlisten.forEach((u) => u()));
watch(() => props.runId, load);
</script>

<template>
  <aside :class="['panel', { expanded: panelWide }]">
    <header class="panel-head">
      <div>
        <h2>{{ heading ?? `Review ${pr?.repo}#${pr?.bb_id}` }}</h2>
        <div class="muted small">{{ subtitle ?? pr?.title }}</div>
      </div>
      <div class="actions">
        <button class="secondary" :aria-pressed="panelWide" @click="togglePanelWide">{{ panelWide ? "Recolher" : "Expandir" }}</button>
        <button class="secondary" @click="emit('close')">Fechar</button>
      </div>
    </header>
    <slot name="switch" />

    <nav v-if="run && run.status !== 'running'" class="panel-tabs">
      <button :class="['tab', { active: view === 'review' }]" @click="view = 'review'">Review</button>
      <button :class="['tab', { active: view === 'chat' }]" @click="openChat">
        Conversa
        <span v-if="chatBusy" class="spinner" />
        <span v-else-if="chat.length" class="tab-count">{{ chat.filter((m) => m.role === "user").length }}</span>
      </button>
      <button :class="['tab', { active: view === 'diff' }]" @click="openDiff">Diff</button>
    </nav>

    <p v-if="error" class="msg error panel-error">{{ error }}</p>

    <div v-show="view === 'review'" class="panel-scroll">
      <template v-if="run">
        <div class="status-line">
          <span :class="['badge', run.status]">{{ run.status }}</span>
          <span>{{ run.stage }}</span>
          <span class="muted small">
            card {{ run.card_key ?? "—" }} · início {{ formatDate(run.started_at) }}
            <template v-if="run.cost_usd !== null"> · US$ {{ run.cost_usd.toFixed(2) }}</template>
          </span>
          <button v-if="run.status === 'running'" class="secondary" @click="cancel">Cancelar</button>
        </div>
        <p v-if="run.error" class="msg error">{{ run.error }}</p>
        <p v-if="run.warning" class="msg warn">Aviso: {{ run.warning }}</p>
        <PipelineView :run="run" />
        <StepLog v-if="run.status === 'running'" :run="run" />

        <template v-if="run.status !== 'running'">
          <h3 class="with-action">
            <span>Achados ({{ main.length }})</span>
            <span class="head-actions">
              <button v-if="main.length" class="secondary small-btn" @click="copyAll">
                {{ copied === "todos" ? "Copiado ✓" : "Copiar todos" }}
              </button>
              <button v-if="fixable && toFix(main).length" class="small-btn" @click="emit('fix', toFix(main))">
                Corrigir todos ({{ toFix(main).length }})
              </button>
            </span>
          </h3>
          <h3 class="sub">
            <span v-if="run.audit_status === 'completed'" class="muted small">— auditados pelo Opus</span>
            <span v-else-if="run.audit_status === 'not_needed'" class="muted small">— Opus não acionado (sem bugs válidos)</span>
            <span v-else-if="run.audit_status === 'failed'" class="muted small">— auditoria do Opus falhou</span>
          </h3>
          <p v-if="run.status === 'completed' && !main.length && !run.warning" class="msg ok">Nenhum bug encontrado.</p>
          <div v-if="pr && run.status === 'completed' && run.rc_status" class="card">
            <strong>Pedido de mudanças</strong>
            <template v-if="run.rc_status === 'completed'">
              <p v-if="run.rc_summary">{{ run.rc_summary }}</p>
              <p class="muted small">
                Validado pelo Sonnet: {{ rcItems.filter((f) => f.rc_decision === "bloqueante").length }} bloqueante(s),
                {{ rcItems.filter((f) => f.rc_decision === "sugestao").length }} sugestão(ões), {{ rcOut.length }} fora. Desmarque o que não quiser enviar.
              </p>
              <div v-for="f in rcItems" :key="f.seq" class="rc-item">
                <label class="small">
                  <input type="checkbox" :checked="!rcSkip.includes(f.seq)" :disabled="!!run.rc_sent_url" @change="toggleRcSkip(f.seq)" />
                  <span :class="['badge', { error: f.rc_decision === 'bloqueante' }]">{{ f.rc_decision === "bloqueante" ? "bloqueante" : "sugestão" }}</span>
                  <a href="#" class="fid" @click.prevent="goFinding(f.seq)">F{{ f.seq }}</a> · <code>{{ f.file }}:{{ f.line }}</code>
                  <template v-if="f.in_diff === false"> · fora do diff (vai no corpo do review)</template>
                </label>
                <pre class="suggestion">{{ f.rc_body }}</pre>
                <p v-if="rcDuplicates(f.seq).length" class="muted small">Junta {{ rcDuplicates(f.seq).join(", ") }}, com a mesma causa.</p>
                <p v-if="f.rc_reason" class="muted small"><em>Por quê:</em> {{ f.rc_reason }}</p>
              </div>
              <p v-if="!rcItems.length" class="msg ok">O agente não achou nada que justifique pedir mudanças.</p>
              <details v-if="rcOut.length">
                <summary class="small">{{ rcOut.length }} achado(s) fora do pedido</summary>
                <ul>
                  <li v-for="f in rcOut" :key="f.seq" class="small">
                    <a href="#" class="fid" @click.prevent="goFinding(f.seq)">F{{ f.seq }}</a> {{ f.title }} —
                    <em>{{ f.rc_duplicate_of ? `repete o F${f.rc_duplicate_of}` : f.rc_reason }}</em>
                  </li>
                </ul>
              </details>
              <p v-if="run.rc_sent_url" class="msg ok">
                Enviado. <a href="#" @click.prevent="openUrl(run.rc_sent_url)">Abrir no GitHub</a>
              </p>
              <button v-else-if="rcItems.length" :disabled="sendingRc || !rcSelected.length" @click="sendRc">
                {{ sendingRc ? "Enviando…" : rcBlocking ? `Pedir mudanças (${rcSelected.length})` : `Comentar (${rcSelected.length})` }}
              </button>
            </template>
            <p v-else-if="run.rc_status === 'not_needed'" class="muted small">Nada a pedir: nenhum achado válido.</p>
            <p v-else-if="run.rc_status === 'failed'" class="msg warn">O agente do pedido de mudanças falhou; use a seleção manual abaixo.</p>
            <p v-else class="muted small">O agente não terminou ({{ run.rc_status }}); use a seleção manual abaixo.</p>
          </div>
          <div v-if="canPublish && main.length" class="card">
            <strong>Publicar achados na PR</strong>
            <p class="muted small">Selecione os achados abaixo. Confira o texto antes de enviar; o Crivo verifica novamente o commit da PR.</p>
            <pre v-if="selectedForPublish.length" class="suggestion">{{ selectedText }}</pre>
            <button :disabled="publishing || !selectedForPublish.length" @click="publish(false)">Publicar comentário</button>
            <button class="secondary" :disabled="publishing || !selectedForPublish.length" @click="publish(true)">Pedir mudanças</button>
            <p v-if="publishMessage" class="msg ok">{{ publishMessage }}</p>
          </div>

          <article v-for="f in main" :id="`finding-${f.seq}`" :key="f.seq" :class="['finding', { flash: flashSeq === f.seq }]">
            <div class="finding-head">
              <span :class="['sev', effectiveSeverity(f)]">{{ effectiveSeverity(f) }}</span>
              <strong>{{ f.title }}</strong>
              <label v-if="canPublish" class="small"><input type="checkbox" :checked="selectedForPublish.includes(f.seq)" :disabled="publishedHere.includes(f.seq)" @change="togglePublish(f.seq)" /> {{ publishedHere.includes(f.seq) ? "Publicado" : "Publicar" }}</label>
              <button class="secondary small-btn push" @click="copy(findingText(f), `F${f.seq}`)">
                {{ copied === `F${f.seq}` ? "Copiado ✓" : "Copiar" }}
              </button>
              <template v-if="fixable">
                <span v-if="fixState?.[f.seq] && fixState[f.seq] !== 'failed'" class="badge">{{ fixLabel[fixState[f.seq]] }}</span>
                <button v-else class="small-btn" @click="emit('fix', [f.seq])">Corrigir</button>
              </template>
            </div>
            <div class="muted small">
              <span class="fid">F{{ f.seq }}</span> · <code>{{ f.file }}:{{ f.line }}</code> · {{ f.project }} · {{ f.kind }} ·
              {{ f.confidence === "plausible" ? "plausível (depende de dados)" : "confirmado" }}
              <template v-if="f.in_diff === false"> · fora do diff (quebra induzida)</template>
            </div>
            <p>{{ f.description }}</p>
            <ol v-if="f.steps.length" class="steps">
              <li v-for="(s, i) in f.steps" :key="i">{{ s }}</li>
            </ol>
            <p v-if="f.pattern_ref" class="small"><em>Padrão do projeto:</em> {{ f.pattern_ref }}</p>
            <p v-if="f.impact" class="small"><em>Impacto:</em> {{ f.impact }}</p>
            <p v-if="f.severity_reason" class="small muted"><em>Severidade:</em> {{ f.severity_reason }}</p>
            <div v-if="stamp(f)" class="verdict">
              <span :class="['stamp', stamp(f)!.tone]">{{ stamp(f)!.text }}</span>
              <span class="small">{{ f.audit_rationale }}</span>
            </div>
          </article>

          <article v-for="f in falsePositives" :id="`finding-${f.seq}`" :key="'fp' + f.seq" :class="['finding', 'fp', { flash: flashSeq === f.seq }]">
            <div class="finding-head">
              <span :class="['sev', f.severity]">{{ f.severity }}</span>
              <strong class="fp-title">{{ f.title }}</strong>
            </div>
            <div class="muted small">
              <span class="fid">F{{ f.seq }}</span> · <code>{{ f.file }}:{{ f.line }}</code> · {{ f.project }} · {{ f.kind }}
            </div>
            <div class="verdict">
              <span class="stamp al">Falso positivo</span>
              <span class="small">{{ f.audit_rationale }}</span>
            </div>
          </article>

          <h3 class="with-action">
            <span>Melhorias de código ({{ improvements.length }})</span>
            <span class="head-actions">
              <button v-if="improvements.length" class="secondary small-btn" @click="copyAllImprovements">
                {{ copied === "melhorias" ? "Copiado ✓" : "Copiar todas" }}
              </button>
              <button v-if="fixable && toFix(improvements).length" class="small-btn" @click="emit('fix', toFix(improvements))">
                Corrigir todas ({{ toFix(improvements).length }})
              </button>
            </span>
          </h3>
          <h3 class="sub">
            <span class="muted small">— complexidade desnecessária, desempenho e testes quebrados no código do PR (Haiku), com nota do juiz (Sonnet); não são bugs</span>
          </h3>
          <p v-if="run.quality_status === 'failed'" class="msg warn">A análise de melhorias falhou; veja o log.</p>
          <p v-else-if="run.quality_status === null" class="muted small">Não rodou neste review.</p>
          <p v-else-if="run.quality_status === 'completed' && !improvements.length" class="muted small">Nenhuma melhoria encontrada.</p>
          <article v-for="f in improvements" :id="`finding-${f.seq}`" :key="f.seq" :class="['finding', 'improvement', { flash: flashSeq === f.seq }]">
            <div class="finding-head">
              <span class="sev melhoria">{{ categoryLabel[f.category ?? ""] ?? "melhoria" }}</span>
              <strong>{{ f.title }}</strong>
              <button class="secondary small-btn push" @click="copy(findingText(f), `F${f.seq}`)">
                {{ copied === `F${f.seq}` ? "Copiado ✓" : "Copiar" }}
              </button>
              <template v-if="fixable">
                <span v-if="fixState?.[f.seq] && fixState[f.seq] !== 'failed'" class="badge">{{ fixLabel[fixState[f.seq]] }}</span>
                <button v-else class="small-btn" @click="emit('fix', [f.seq])">Corrigir</button>
              </template>
            </div>
            <div class="muted small">
              <span class="fid">F{{ f.seq }}</span> · <code>{{ f.file }}:{{ lineRange(f) }}</code> · {{ f.project }} ·
              {{ f.category === "desempenho" || f.category === "teste" ? f.category : "melhoria técnica" }}
            </div>
            <div class="score" :aria-label="f.score !== null ? `Nota do juiz: ${f.score} de 10` : 'Sem nota do juiz'">
              <span class="score-bar" aria-hidden="true"><i v-for="n in 10" :key="n" :class="{ f: f.score !== null && n <= f.score }" /></span>
              <b v-if="f.score !== null">{{ f.score }}/10</b>
              <b v-else class="muted">sem nota</b>
              <span v-if="f.importance" :class="['imp', f.importance]">{{ importanceLabel[f.importance] }}</span>
            </div>
            <p>{{ f.description }}</p>
            <p v-if="f.evidence" class="small"><em>Por quê:</em> {{ f.evidence }}</p>
            <DiffView v-if="diffOf(f)" :edit="diffOf(f)!" />
            <template v-else-if="f.existing_code !== null">
              <!-- desempenho ou teste sem código novo: a correção mexe em mais de um método e fica na descrição -->
              <p class="small"><em>Atual:</em></p>
              <pre class="suggestion">{{ f.existing_code }}</pre>
            </template>
            <template v-else-if="f.suggestion">
              <!-- formato livre, de antes da migration 0008 -->
              <p class="small"><em>Mais simples:</em></p>
              <pre class="suggestion">{{ f.suggestion }}</pre>
            </template>
            <p v-if="f.score_why" class="audit small">
              <strong>Juiz:</strong> {{ f.score_why }}
            </p>
          </article>

          <div v-if="discarded.length" class="discarded">
            <button class="link" @click="showRejected = !showRejected">
              {{ showRejected ? "Ocultar" : "Ver" }} {{ discarded.length }} descartado(s)
            </button>
            <ul v-if="showRejected">
              <li v-for="f in discarded" :key="f.seq" class="small">
                <code>{{ f.file }}:{{ f.line }}</code> {{ f.title }} — <em>{{ f.reject_reason }}</em>
              </li>
            </ul>
          </div>
        </template>

        <template v-if="run.browser_status">
          <h3>
            Testes no navegador
            <span :class="['badge', run.browser_status]">{{ browserLabel[run.browser_status] }}</span>
          </h3>
          <div v-if="run.browser_note" :class="['small', run.browser_status === 'blocked' ? 'msg warn' : 'muted']">
            {{ run.browser_note }}
            <div v-if="run.browser_status === 'blocked' && run.browser_checkout_pr_id" class="alert-actions">
              <button v-if="!checkoutDone" class="small-btn" :disabled="checkoutBusy" @click="checkoutFromAlert">
                {{ checkoutBusy ? "Trocando…" : `Checkout ${run.browser_checkout_label ?? ""}` }}
              </button>
              <template v-else>
                <span>{{ checkoutDone }}</span>
                <button class="small-btn" @click="rerun">Revisar de novo</button>
              </template>
            </div>
          </div>
          <article v-for="t in run.browser_tests" :key="t.seq" class="finding">
            <div class="finding-head">
              <span :class="['tstat', t.status]">{{ testLabel[t.status] }}</span>
              <strong>{{ t.title }}</strong>
              <span v-if="t.finding_ref" class="muted small">(achado {{ t.finding_ref }})</span>
            </div>
            <div class="muted small"><code>{{ t.url }}</code></div>
            <ol v-if="t.actions.length" class="steps small">
              <li v-for="(a, i) in t.actions" :key="i">{{ a }}</li>
            </ol>
            <p class="small"><em>Esperado:</em> {{ t.expected }}</p>
            <p class="small"><em>Observado:</em> {{ t.observed }}</p>
            <p v-if="t.evidence" class="small muted"><em>Evidência:</em> {{ t.evidence }}</p>
          </article>
        </template>

        <template v-if="run.status !== 'running'">
          <h3>Etapas</h3>
          <StepLog :run="run" />
        </template>
        <details class="log-section" :open="run.status === 'running'">
          <summary>Log completo</summary>
          <pre ref="logBox" class="log">{{ run.log.join("\n") || "aguardando…" }}</pre>
          <p class="muted small">Arquivos: <code>{{ run.run_dir }}</code></p>
        </details>
      </template>
    </div>

    <!-- conversa: aba própria, com a altura toda do painel -->
    <div v-if="run && run.status !== 'running'" v-show="view === 'chat'" class="chat-pane">
      <div ref="chatScroll" class="chat-scroll">
        <div v-if="!chat.length && !chatBusy" class="chat-empty">
          <p class="muted">
            Pergunte sobre os achados ou sobre outros bugs da task. O Sonnet lê o código do commit revisado, só para
            leitura, e o card do Jira.
          </p>
          <div class="chips">
            <button v-for="s in suggestions" :key="s" type="button" class="chip-btn" @click="ask(s)">{{ s }}</button>
          </div>
        </div>
        <section v-else class="thread">
          <template v-for="m in chat" :key="m.id">
            <div v-if="m.role === 'user'" class="msg-user">{{ m.content }}</div>
            <div v-else-if="m.role === 'assistant'" class="msg-claude">
              <div class="msg-claude-head">Resposta</div>
              <!-- eslint-disable-next-line vue/no-v-html -->
              <div class="md" v-html="renderMd(m.content)" />
              <div class="msg-actions">
                <button class="icon-btn" @click="copy(m.content, `chat${m.id}`)">
                  {{ copied === `chat${m.id}` ? "✓ copiado" : "⧉ copiar" }}
                </button>
                <span v-if="m.cost_usd !== null" class="muted">US$ {{ m.cost_usd.toFixed(2) }}</span>
              </div>
            </div>
            <div v-else class="msg-error">{{ m.content }}</div>
          </template>
          <div v-if="chatBusy" class="msg-claude">
            <div class="msg-claude-head"><span class="spinner" /> Analisando o código<span class="dots" /></div>
            <details v-if="chatProgress.length" class="steps-box">
              <summary>{{ chatProgress.length }} passo(s) — {{ chatProgress[chatProgress.length - 1] }}</summary>
              <div v-for="(l, i) in chatProgress" :key="i" class="step-line">{{ l }}</div>
            </details>
          </div>
        </section>
      </div>
      <div class="composer-dock">
        <form class="composer" @submit.prevent="ask()" @click="focusInput">
          <textarea
            ref="input"
            v-model="question"
            rows="3"
            placeholder="Pergunte sobre este review…"
            @input="autosize"
            @keydown.enter.exact.prevent="ask()"
          />
          <div class="composer-bar">
            <span class="model-chip">Sonnet · só leitura · commit revisado</span>
            <button v-if="!chatBusy" type="submit" class="send-btn" :disabled="!question.trim()" aria-label="Enviar">
              <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
                <path d="M12 19V5M5 12l7-7 7 7" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" />
              </svg>
            </button>
            <button v-else type="button" class="send-btn stop" aria-label="Parar" @click="stopChat">
              <svg viewBox="0 0 24 24" width="12" height="12" aria-hidden="true"><rect x="5" y="5" width="14" height="14" rx="2" fill="currentColor" /></svg>
            </button>
          </div>
        </form>
      </div>
    </div>

    <PrDiff v-if="run && run.status !== 'running' && diffOpened" v-show="view === 'diff'" :run-id="runId" :findings="run.findings" @open-finding="goFinding" />
  </aside>
</template>
