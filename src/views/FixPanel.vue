<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { api, errorText, events, formatDate, type FixComment, type FixSession, type Pr } from "../api";
import DiffView from "./DiffView.vue";
import { panelWide, togglePanelWide } from "../panelWide";

/** Meus PRs (`pr`: comentários dos revisores) ou Minhas branches (`branch`: achados do review). */
const props = defineProps<{ pr?: Pr; branch?: { slug: string; name: string } }>();
const emit = defineEmits<{ close: [] }>();
const isPr = computed(() => !!props.pr);
const providerName = computed(() => props.pr?.provider === "github" ? "GitHub" : "Bitbucket");
const branchName = computed(() => props.pr?.source_branch ?? props.branch?.name ?? "");

const fix = ref<FixSession | null>(null);
const loaded = ref(false);
const error = ref("");
const notice = ref("");
/** Linhas ao vivo do Claude (análise, conversa ou committer); somem quando termina. */
const progress = ref<string[]>([]);
const logBox = ref<HTMLElement | null>(null);
/** Aba ativa: a lista de comentários ou a conversa sobre um deles (id do comentário). */
const view = ref<"list" | number>("list");
/** Conversas abertas, uma aba por comentário, na ordem em que foram abertas. */
const chatIds = ref<number[]>([]);
const confirmRestart = ref("");
const approving = ref<number | null>(null);
const copied = ref("");
const unlisten: (() => void)[] = [];

async function load() {
  // rola a conversa aberta até o fim quando ela ganha mensagem ou muda de status
  const sig = () => (current.value ? `${current.value.id}:${current.value.messages.length}:${current.value.status}` : "");
  const before = sig();
  try {
    fix.value = props.pr
      ? await api.fixGet(props.pr.id)
      : await api.fixGetBranch(props.branch?.slug ?? "", props.branch?.name ?? "");
    // o log fica depois do fim da execução (o erro pode pedir "veja o log"); cada ação o limpa ao começar
    watchTerminal();
    chatIds.value = chatIds.value.filter((id) => comments.value.some((c) => c.id === id));
    if (view.value !== "list" && !chatIds.value.includes(view.value)) view.value = "list";
    if (current.value && sig() !== before) scrollToBottom();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    loaded.value = true;
  }
}

const busy = computed(() => !!fix.value?.busy);

/** Correção rodando pelo terminal (`comments`): os eventos não chegam de outro processo, então relê. */
let terminalTimer: ReturnType<typeof setInterval> | undefined;
function watchTerminal() {
  if (fix.value?.terminal && !terminalTimer) terminalTimer = setInterval(load, 3_000);
  if (!fix.value?.terminal && terminalTimer) {
    clearInterval(terminalTimer);
    terminalTimer = undefined;
  }
}
const comments = computed(() => fix.value?.comments ?? []);
const current = computed(() => (view.value === "list" ? null : comments.value.find((c) => c.id === view.value) ?? null));
const chats = computed(() =>
  chatIds.value.map((id) => comments.value.find((c) => c.id === id)).filter((c): c is FixComment => !!c),
);
const resolved = (c: FixComment) => c.status === "applied" || c.status === "nao_procede" || c.status === "resposta";
/** Não analisado ('new') não bloqueia o commit: o usuário escolhe quais comentários tratar. */
const unresolved = computed(() => comments.value.filter((c) => !resolved(c) && c.status !== "new"));
const notAnalyzed = computed(() => comments.value.filter((c) => c.status === "new"));
const analyzing = ref(false);

async function analyzeItems(ids: number[]) {
  error.value = "";
  notice.value = "";
  analyzing.value = true;
  progress.value = [];
  try {
    await api.fixAnalyze(ids);
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    analyzing.value = false;
  }
}

async function refresh() {
  if (!fix.value) return;
  error.value = "";
  notice.value = "";
  try {
    await api.fixRefresh(fix.value.id);
    await load();
  } catch (e) {
    error.value = errorText(e);
  }
}
const appliedCount = computed(() => comments.value.filter((c) => c.applied_at).length);
/** F3 para achado do review, C3 para comentário do PR. */
const label = (c: FixComment) => (c.finding_seq !== null ? `F${c.finding_seq}` : `C${c.seq}`);
const canCommit = computed(
  () => fix.value?.status === "ready" && !busy.value && comments.value.length > 0 && !unresolved.value.length && appliedCount.value > 0,
);

const statusLabel: Record<FixComment["status"], string> = {
  new: "não analisado",
  queued: "na fila",
  analyzing: "analisando",
  procede: "sugestão",
  nao_procede: "não procede",
  resposta: "resposta",
  applied: "aplicado",
  failed: "falhou",
};
const statusClass: Record<FixComment["status"], string> = {
  new: "",
  queued: "",
  analyzing: "",
  procede: "bloqueado",
  nao_procede: "",
  resposta: "",
  applied: "aprovado",
  failed: "reprovado",
};
const sessionLabel: Record<FixSession["status"], string> = {
  analyzing: "analisando",
  ready: "pronto",
  failed: "falhou",
  cancelled: "cancelado",
  committing: "commit",
  committed: "commit feito",
  pushed: "enviado",
};
const sessionBadge = (s: FixSession["status"]) =>
  s === "pushed" || s === "committed" ? "completed" : s === "analyzing" || s === "committing" ? "running" : s === "ready" ? "" : s;

const place = (c: FixComment) => (c.path ? `${c.path}${c.line ? `:${c.line}` : ""}` : "comentário geral");

/** Comentário em Markdown → HTML sanitizado (o texto vem do Bitbucket ou do Claude). */
const renderMd = (text: string) => DOMPurify.sanitize(marked.parse(text, { async: false, gfm: true, breaks: true }) as string);

function scrollLog() {
  nextTick(() => {
    if (logBox.value) logBox.value.scrollTop = logBox.value.scrollHeight;
  });
}

async function start(force = false) {
  if (!props.pr) return;
  error.value = "";
  notice.value = "";
  confirmRestart.value = "";
  progress.value = [];
  try {
    await api.fixStart(props.pr.id, force);
    view.value = "list";
    chatIds.value = [];
    await load();
  } catch (e) {
    const msg = errorText(e);
    // sugestões aprovadas e sem commit (fix::check_previous): pede confirmação antes de descartar
    if (msg.includes("descarta essas alterações")) confirmRestart.value = msg;
    else error.value = msg;
  }
}

async function cancel() {
  if (!fix.value) return;
  try {
    await api.fixCancel(fix.value.id);
  } catch (e) {
    error.value = errorText(e);
  }
}

async function approve(c: FixComment) {
  error.value = "";
  notice.value = "";
  approving.value = c.id;
  try {
    const files = await api.fixApprove(c.id);
    notice.value = `${label(c)} aplicado no worktree da correção: ${files.join(", ")}.`;
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    approving.value = null;
  }
}

async function commitPush() {
  if (!fix.value) return;
  error.value = "";
  notice.value = "";
  progress.value = [];
  try {
    await api.fixCommit(fix.value.id);
    await load();
  } catch (e) {
    error.value = errorText(e);
  }
}

const pushing = ref(false);

async function push() {
  if (!fix.value) return;
  error.value = "";
  pushing.value = true;
  progress.value = [];
  try {
    await api.fixPush(fix.value.id);
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    pushing.value = false;
  }
}

const confirmDiscard = ref(false);

async function discard() {
  if (!fix.value) return;
  error.value = "";
  confirmDiscard.value = false;
  try {
    await api.fixDiscard(fix.value.id);
    await load();
  } catch (e) {
    error.value = errorText(e);
  }
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

// ---- conversas sobre os comentários: uma aba por comentário ----
const chatScroll = ref<HTMLElement | null>(null);
const input = ref<HTMLTextAreaElement | null>(null);
const tabsNav = ref<HTMLElement | null>(null);
const pinnedTab = ref<HTMLElement | null>(null);
const tabEls = new Map<number, HTMLElement>();
/** Rascunho de cada conversa: trocar de aba não perde o que foi digitado. */
const drafts = ref<Record<number, string>>({});
/** Posição da rolagem de cada conversa, para voltar à aba onde o usuário parou. */
const scrollPos = new Map<number, number>();

/** Troca de aba guardando a rolagem da conversa que sai de cena. */
function switchTo(next: "list" | number) {
  if (current.value && chatScroll.value) scrollPos.set(current.value.id, chatScroll.value.scrollTop);
  view.value = next;
}

function bindTab(id: number) {
  return (el: unknown) => {
    if (el) tabEls.set(id, el as HTMLElement);
    else tabEls.delete(id);
  };
}

function scrollToBottom(smooth = true) {
  nextTick(() => chatScroll.value?.scrollTo({ top: chatScroll.value.scrollHeight, behavior: smooth ? "smooth" : "auto" }));
}

/** Rola a barra de abas até a aba da conversa, sem deixá-la por baixo da aba fixa da lista. */
function revealTab(id: number) {
  const nav = tabsNav.value;
  const el = tabEls.get(id);
  if (!nav || !el) return;
  const left = el.offsetLeft - (pinnedTab.value?.offsetWidth ?? 0);
  const right = el.offsetLeft + el.offsetWidth - nav.clientWidth;
  if (nav.scrollLeft > left) nav.scrollTo({ left, behavior: "smooth" });
  else if (nav.scrollLeft < right) nav.scrollTo({ left: right, behavior: "smooth" });
}

/** Abre a conversa numa aba nova (ou volta para a aba dela, se já estiver aberta). */
function openChat(c: FixComment) {
  if (!chatIds.value.includes(c.id)) chatIds.value = [...chatIds.value, c.id];
  if (view.value === c.id) return revealTab(c.id);
  switchTo(c.id);
  const pos = scrollPos.get(c.id);
  if (pos === undefined) scrollToBottom(false);
  nextTick(() => {
    if (pos !== undefined && chatScroll.value) chatScroll.value.scrollTop = pos;
    revealTab(c.id);
    autosize();
    input.value?.focus();
  });
}

/** Fecha a aba; se era a ativa, mostra a vizinha, ou a lista quando não sobra nenhuma. */
function closeChat(id: number) {
  const i = chatIds.value.indexOf(id);
  chatIds.value = chatIds.value.filter((x) => x !== id);
  scrollPos.delete(id);
  if (view.value !== id) return;
  view.value = "list"; // sem guardar a rolagem da aba que fechou
  const next = chats.value[i] ?? chats.value[i - 1];
  if (next) openChat(next);
}

function focusInput(e: MouseEvent) {
  if (!(e.target as HTMLElement).closest("button, textarea")) input.value?.focus();
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

async function send(text?: string) {
  const c = current.value;
  const t = (text ?? (c ? drafts.value[c.id] : "") ?? "").trim();
  if (!t || !c || busy.value) return;
  error.value = "";
  progress.value = [];
  try {
    await api.fixAsk(c.id, t);
    drafts.value[c.id] = "";
    nextTick(autosize);
    await load();
    scrollToBottom();
  } catch (e) {
    error.value = errorText(e);
  }
}

const chatSuggestions = computed(() => {
  const c = current.value;
  if (!c) return [];
  if (c.status === "procede") return ["Explique a correção", "Faça a correção mais simples"];
  if (c.status === "nao_procede") return ["O revisor tem razão: corrija", "Mostre a evidência no código"];
  if (c.status === "failed") return ["Proponha a correção de novo, a partir do código atual"];
  return ["Explique de novo"];
});

/** O terminal não manda eventos para o app: relê ao voltar para a janela. */
const onFocus = () => load();

onMounted(async () => {
  await load();
  window.addEventListener("focus", onFocus);
  unlisten.push(
    await events.onFixUpdate((e) => {
      const mine = props.pr ? e.pr_id === props.pr.id : e.repo_slug === props.branch?.slug && e.branch === props.branch?.name;
      if (!mine) return;
      if (e.line) {
        progress.value = [...progress.value, e.line];
        if (fix.value) fix.value.busy = true;
        scrollLog();
        if (current.value?.status === "analyzing") scrollToBottom();
      } else {
        load();
      }
    }),
  );
});
onUnmounted(() => {
  unlisten.forEach((u) => u());
  if (terminalTimer) clearInterval(terminalTimer);
  window.removeEventListener("focus", onFocus);
});
</script>

<template>
  <aside :class="['panel', { expanded: panelWide }]">
    <header class="panel-head">
      <div>
        <h2 v-if="pr">Comentários {{ pr.repo }}#{{ pr.bb_id }}</h2>
        <h2 v-else>Correções {{ branch?.name }}</h2>
        <div class="muted small">{{ pr ? pr.title : branch?.slug }}</div>
      </div>
      <div class="actions">
        <button class="secondary" :aria-pressed="panelWide" @click="togglePanelWide">{{ panelWide ? "Recolher" : "Expandir" }}</button>
        <button class="secondary" @click="emit('close')">Fechar</button>
      </div>
    </header>
    <slot name="switch" />

    <!-- abas: a lista (fixa à esquerda) e uma conversa por comentário; a barra rola na horizontal -->
    <nav v-if="fix && comments.length" ref="tabsNav" class="panel-tabs tabs-scroll">
      <span ref="pinnedTab" class="tab-pin">
        <button :class="['tab', { active: view === 'list' }]" @click="switchTo('list')">
          {{ isPr ? "Comentários" : "Itens" }} <span class="tab-count">{{ comments.length - unresolved.length }}/{{ comments.length }}</span>
        </button>
      </span>
      <span v-for="c in chats" :key="c.id" :ref="bindTab(c.id)" :class="['chat-tab', { active: view === c.id }]">
        <button class="tab" @click="openChat(c)">
          Conversa · {{ label(c) }}
          <span v-if="c.status === 'analyzing'" class="spinner" />
        </button>
        <button class="tab-close" :aria-label="`Fechar a conversa ${label(c)}`" title="Fechar" @click="closeChat(c.id)">×</button>
      </span>
    </nav>

    <p v-if="error" class="msg error panel-error">{{ error }}</p>
    <p v-if="notice" class="msg ok panel-error">{{ notice }}</p>
    <div v-if="confirmRestart" class="msg warn panel-error">
      {{ confirmRestart }}.
      <div class="alert-actions">
        <button class="small-btn" @click="start(true)">Descartar e analisar de novo</button>
        <button class="small-btn" @click="confirmRestart = ''">Cancelar</button>
      </div>
    </div>

    <!-- lista de comentários -->
    <div v-show="view === 'list'" class="panel-scroll">
      <p v-if="loaded && !fix && !isPr" class="muted">
        Nenhuma correção ainda. Na aba Review, clique em Corrigir num achado ou numa melhoria: o Sonnet confere no código e
        propõe a correção, que só é gravada quando você aprovar.
      </p>
      <template v-if="loaded && !fix && pr">
        <p class="muted">
          Busca os comentários abertos deste PR no {{ providerName }} ({{ pr.comment_count ?? "?" }} no total, contando os seus),
          sem rodar o Claude. Depois você escolhe quais analisar: o Sonnet confere cada um no código da branch e propõe a
          correção quando procede. Nada muda no código até você aprovar a sugestão.
        </p>
        <button @click="start()">Buscar comentários</button>
      </template>

      <template v-if="fix">
        <div class="status-line">
          <span :class="['badge', sessionBadge(fix.status)]">{{ sessionLabel[fix.status] }}</span>
          <span>{{ fix.stage }}</span>
          <span class="muted small">
            card {{ fix.card_key ?? "—" }} · início {{ formatDate(fix.created_at) }} · US$ {{ fix.cost_usd.toFixed(2) }}
          </span>
          <span v-if="fix.terminal" class="muted small">rodando pelo terminal</span>
          <button v-else-if="busy" class="secondary" @click="cancel">Cancelar</button>
          <template v-else-if="isPr && fix.status === 'ready'">
            <button class="secondary" @click="refresh">Buscar novos comentários</button>
            <button class="secondary" @click="start()">Começar de novo</button>
          </template>
          <button v-else-if="isPr && fix.status !== 'committing'" class="secondary" @click="start()">Buscar comentários de novo</button>
          <button
            v-else-if="!isPr && (fix.status === 'ready' || fix.status === 'failed')"
            class="secondary"
            @click="confirmDiscard = true"
          >
            Descartar
          </button>
        </div>
        <div v-if="confirmDiscard" class="msg warn">
          Descartar apaga o worktree desta correção, com as sugestões aprovadas e ainda sem commit. A branch não muda.
          <div class="alert-actions">
            <button class="small-btn" @click="discard">Descartar</button>
            <button class="small-btn" @click="confirmDiscard = false">Cancelar</button>
          </div>
        </div>
        <p v-if="fix.error" class="msg error">{{ fix.error }}</p>
        <p v-if="fix.status === 'pushed' && isPr" class="msg ok">
          Enviado para origin/{{ branchName }} em {{ formatDate(fix.pushed_at) }}:
          <code>{{ fix.commit_sha?.slice(0, 10) }}</code> {{ fix.commit_subject }}. O seu clone local continua no commit
          anterior; faça um <code>git pull</code> na branch para trazer.
        </p>
        <p v-if="fix.status === 'pushed' && !isPr" class="msg ok">
          <code>{{ fix.commit_sha?.slice(0, 10) }}</code> {{ fix.commit_subject }} enviado para origin/{{ branchName }} em
          {{ formatDate(fix.pushed_at) }}.
        </p>
        <div v-if="fix.status === 'committed'" class="msg ok">
          Commit na branch local {{ branchName }}: <code>{{ fix.commit_sha?.slice(0, 10) }}</code> {{ fix.commit_subject }}.
          <div class="alert-actions">
            <button class="small-btn" :disabled="pushing" @click="push">
              {{ pushing ? "Enviando…" : `Push para origin/${branchName}` }}
            </button>
          </div>
        </div>
        <p v-if="fix.status === 'ready' && !comments.length && isPr" class="msg ok">
          Nenhum comentário aberto de outra pessoa neste PR.
        </p>
        <div v-if="notAnalyzed.length && (fix.status === 'ready' || fix.status === 'analyzing')" class="alert-actions">
          <span class="muted small">
            {{ notAnalyzed.length }} não analisado(s). Clique em Analisar em cada um, ou analise todos; a fila roda um por vez.
          </span>
          <button
            class="secondary small-btn"
            :disabled="analyzing || fix.terminal"
            @click="analyzeItems(notAnalyzed.map((c) => c.id))"
          >
            Analisar todos ({{ notAnalyzed.length }})
          </button>
        </div>

        <article v-for="c in comments" :key="c.id" class="finding">
          <div class="finding-head">
            <span :class="['tstat', statusClass[c.status]]">
              <span v-if="c.status === 'analyzing'" class="spinner" /> {{ statusLabel[c.status] }}
            </span>
            <strong>{{ label(c) }} · {{ c.author }}</strong>
            <button
              v-if="c.explanation"
              class="secondary small-btn push"
              @click="copy(c.explanation, `C${c.id}`)"
            >
              {{ copied === `C${c.id}` ? "Copiado ✓" : "Copiar" }}
            </button>
            <button
              v-if="c.status === 'new' || c.status === 'failed'"
              :class="['small-btn', { push: !c.explanation }]"
              :disabled="analyzing || fix.terminal || !(fix.status === 'ready' || fix.status === 'analyzing')"
              @click="analyzeItems([c.id])"
            >
              {{ c.status === "new" ? "Analisar" : "Analisar de novo" }}
            </button>
            <button
              v-if="c.status !== 'new'"
              :class="['secondary', 'small-btn', { push: !c.explanation && c.status !== 'failed' }]"
              @click="openChat(c)"
            >
              Conversar
            </button>
          </div>
          <div class="muted small">
            <code>{{ place(c) }}</code>
            <template v-if="c.outdated"> · código mudou depois do comentário</template>
            · {{ formatDate(c.created_on) }}
            <template v-if="c.url"> · <a href="#" @click.prevent="openUrl(c.url)">abrir no {{ providerName }}</a></template>
          </div>
          <!-- eslint-disable-next-line vue/no-v-html -->
          <blockquote class="comment-body md" v-html="renderMd(c.body)" />
          <div v-for="(r, i) in c.replies" :key="i" class="reply small">
            <strong>{{ r.author }}:</strong> {{ r.body }}
          </div>
          <p v-if="c.explanation" class="explanation">{{ c.explanation }}</p>
          <p v-if="c.error" class="msg error small">{{ c.error }}</p>
          <template v-if="c.edits.length && c.status !== 'nao_procede' && c.status !== 'resposta'">
            <DiffView v-for="(e, i) in c.edits" :key="i" :edit="e" :applied="c.status === 'applied'" />
          </template>
          <div v-if="c.status === 'procede'" class="alert-actions">
            <button :disabled="busy || approving !== null" @click="approve(c)">
              {{ approving === c.id ? "Aplicando…" : "Aprovar e aplicar" }}
            </button>
            <span class="muted small">Grava no worktree da correção, não no seu checkout. Para mudar algo, use Conversar.</span>
          </div>
        </article>

        <section v-if="comments.length && (fix.status === 'ready' || fix.status === 'committing')" class="commit-box">
          <template v-if="unresolved.length">
            <p class="muted small">
              Falta resolver {{ unresolved.map(label).join(", ") }}: aprove a sugestão ou converse até o item ficar como
              "não procede" ou "resposta".
            </p>
          </template>
          <template v-else-if="!appliedCount">
            <p class="muted small">Todos resolvidos sem alteração de código: não há o que commitar.</p>
          </template>
          <template v-else>
            <p v-if="isPr" class="small">
              Todos os comentários analisados estão resolvidos ({{ appliedCount }} com correção aplicada<template
                v-if="notAnalyzed.length">; {{ notAnalyzed.length }} não analisado(s) ficam de fora</template>). O agente
              <code>committer</code> (Haiku) cria o commit e o app faz o push para <code>origin/{{ branchName }}</code>,
              sem force.
            </p>
            <p v-else class="small">
              Todos os itens estão resolvidos ({{ appliedCount }} com correção aplicada). O agente <code>committer</code>
              (Haiku) cria o commit e o app avança a branch local <code>{{ branchName }}</code>; se ela estiver aberta no
              seu checkout, ele precisa estar sem alterações. O push vem depois, num botão à parte.
            </p>
          </template>
          <button :disabled="!canCommit" @click="commitPush">
            <template v-if="fix.status === 'committing'">Commit em andamento…</template>
            <template v-else>{{ isPr ? "Commit e push" : "Commit na branch" }}</template>
          </button>
        </section>

        <details class="log-section" :open="busy && view === 'list'">
          <summary>Log da execução</summary>
          <pre ref="logBox" class="log">{{ progress.join("\n") || (busy ? "aguardando…" : "sem execução em andamento") }}</pre>
          <p class="muted small">Arquivos: <code>{{ fix.run_dir }}</code></p>
        </details>
      </template>
    </div>

    <!-- conversa sobre um comentário: altura toda do painel -->
    <div v-if="current" :key="current.id" class="chat-pane">
      <div ref="chatScroll" class="chat-scroll">
        <article class="finding">
          <div class="finding-head">
            <span :class="['tstat', statusClass[current.status]]">
              <span v-if="current.status === 'analyzing'" class="spinner" /> {{ statusLabel[current.status] }}
            </span>
            <strong>{{ label(current) }} · {{ current.author }}</strong>
            <span class="muted small push"><code>{{ place(current) }}</code></span>
          </div>
          <!-- eslint-disable-next-line vue/no-v-html -->
          <blockquote class="comment-body md" v-html="renderMd(current.body)" />
          <p v-if="current.explanation" class="explanation">{{ current.explanation }}</p>
          <p v-if="current.error" class="msg error small">{{ current.error }}</p>
          <template v-if="current.edits.length && current.status !== 'nao_procede' && current.status !== 'resposta'">
            <DiffView v-for="(e, i) in current.edits" :key="i" :edit="e" :applied="current.status === 'applied'" />
          </template>
          <div v-if="current.status === 'procede'" class="alert-actions">
            <button :disabled="busy || approving !== null" @click="approve(current)">
              {{ approving === current.id ? "Aplicando…" : "Aprovar e aplicar" }}
            </button>
          </div>
        </article>

        <section class="thread">
          <template v-for="m in current.messages" :key="m.id">
            <div v-if="m.role === 'user'" class="msg-user">{{ m.content }}</div>
            <div v-else-if="m.role === 'assistant'" class="msg-claude">
              <div class="msg-claude-head">Resposta</div>
              <!-- eslint-disable-next-line vue/no-v-html -->
              <div class="md" v-html="renderMd(m.content)" />
              <div class="msg-actions">
                <span v-if="m.cost_usd !== null" class="muted">US$ {{ m.cost_usd.toFixed(2) }}</span>
              </div>
            </div>
            <div v-else class="msg-error">{{ m.content }}</div>
          </template>
          <div v-if="current.status === 'analyzing'" class="msg-claude">
            <div class="msg-claude-head"><span class="spinner" /> Conferindo no código<span class="dots" /></div>
            <details v-if="progress.length" class="steps-box">
              <summary>{{ progress.length }} passo(s) — {{ progress[progress.length - 1] }}</summary>
              <div v-for="(l, i) in progress" :key="i" class="step-line">{{ l }}</div>
            </details>
          </div>
          <div v-else-if="!current.messages.length" class="chips">
            <button v-for="s in chatSuggestions" :key="s" type="button" class="chip-btn" @click="send(s)">{{ s }}</button>
          </div>
        </section>
      </div>
      <div class="composer-dock">
        <form class="composer" @submit.prevent="send()" @click="focusInput">
          <textarea
            ref="input"
            v-model="drafts[current.id]"
            rows="3"
            :placeholder="`Peça uma mudança na sugestão do ${label(current)} ou pergunte sobre ela…`"
            :disabled="fix?.status !== 'ready'"
            @input="autosize"
            @keydown.enter.exact.prevent="send()"
          />
          <div class="composer-bar">
            <span class="model-chip">Sonnet · sugere, não grava · você aprova</span>
            <button v-if="!busy" type="submit" class="send-btn" :disabled="!drafts[current.id]?.trim()" aria-label="Enviar">
              <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
                <path d="M12 19V5M5 12l7-7 7 7" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" />
              </svg>
            </button>
            <button v-else type="button" class="send-btn stop" aria-label="Parar" @click="cancel">
              <svg viewBox="0 0 24 24" width="12" height="12" aria-hidden="true"><rect x="5" y="5" width="14" height="14" rx="2" fill="currentColor" /></svg>
            </button>
          </div>
        </form>
      </div>
    </div>
  </aside>
</template>
