<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { api, events, loadJiraSite, type LimitState, type TriggerState } from "./api";
import MyBranches from "./views/MyBranches.vue";
import MyPrs from "./views/MyPrs.vue";
import PrList from "./views/PrList.vue";
import Settings from "./views/Settings.vue";
import Triggers from "./views/Triggers.vue";
import Usage from "./views/Usage.vue";

const tab = ref<"prs" | "mine" | "branches" | "triggers" | "usage" | "settings">("prs");

// ---- limite de uso do Claude (aviso global) ----
const limit = ref<LimitState | null>(null);
let unlisten: (() => void) | undefined;
let timer: ReturnType<typeof setInterval> | undefined;

const clock = (s: number) =>
  new Date(s * 1000).toLocaleString("pt-BR", { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit" });

async function refreshLimit() {
  try {
    limit.value = await api.claudeLimitGet();
  } catch {
    /* sem aviso se a consulta falhar */
  }
}

async function tryNow() {
  await api.claudeLimitClear();
  limit.value = null;
}

// ---- gatilhos rodando (contagem na aba) ----
const triggerStates = ref<Record<string, TriggerState>>({});
const running = computed(() => Object.values(triggerStates.value).filter((s) => s.state === "running").length);
let unlistenTriggers: (() => void) | undefined;

onMounted(async () => {
  // link do card nas listas
  loadJiraSite().catch(() => {});
  try {
    const all = await api.triggersList();
    triggerStates.value = Object.fromEntries(all.map((v) => [v.trigger.id, v.state]));
  } catch {
    /* sem contagem se a consulta falhar */
  }
  unlistenTriggers = await events.onTriggerStatus((e) => {
    triggerStates.value = { ...triggerStates.value, [e.id]: e.state };
  });
  await refreshLimit();
  unlisten = await events.onClaudeLimit((v) => (limit.value = v));
  timer = setInterval(refreshLimit, 30_000);
});
onUnmounted(() => {
  unlisten?.();
  unlistenTriggers?.();
  if (timer) clearInterval(timer);
});
</script>

<template>
  <header class="top">
    <div class="brand">
      <span class="brand-mark" aria-hidden="true">PR</span>
      <h1>PR Review</h1>
    </div>
    <nav class="tabs">
      <button :class="['tab', { active: tab === 'prs' }]" @click="tab = 'prs'">Para eu revisar</button>
      <button :class="['tab', { active: tab === 'mine' }]" @click="tab = 'mine'">Meus PRs</button>
      <button :class="['tab', { active: tab === 'branches' }]" @click="tab = 'branches'">Minhas branches</button>
      <button :class="['tab', { active: tab === 'triggers' }]" @click="tab = 'triggers'">
        Gatilhos <span v-if="running" class="tab-count">{{ running }}</span>
      </button>
      <button :class="['tab', { active: tab === 'usage' }]" @click="tab = 'usage'">Uso</button>
      <button :class="['tab', { active: tab === 'settings' }]" @click="tab = 'settings'">Configurações</button>
    </nav>
  </header>

  <div v-if="limit" class="limit-banner" role="status">
    <span class="limit-icon" aria-hidden="true">◔</span>
    <div class="limit-text">
      <strong>Limite de uso do Claude atingido.</strong>
      <template v-if="limit.reset_at"> Libera às {{ clock(limit.reset_at) }}.</template>
      <template v-else-if="limit.reset_text"> {{ limit.reset_text }}.</template>
      Os reviews ficam pausados e retomam sozinhos (próxima tentativa {{ clock(limit.retry_at) }}); o chat volta quando
      liberar.
      <div class="limit-msg">{{ limit.message }}</div>
    </div>
    <button class="secondary" @click="tryNow">Tentar agora</button>
  </div>

  <main>
    <PrList v-if="tab === 'prs'" @go-mine="tab = 'mine'" />
    <MyPrs v-else-if="tab === 'mine'" />
    <MyBranches v-else-if="tab === 'branches'" />
    <Triggers v-else-if="tab === 'triggers'" />
    <div v-else-if="tab === 'usage'" class="narrow wide"><Usage /></div>
    <div v-else class="narrow"><Settings /></div>
  </main>
</template>

<style>
/* =========================================================================
   Tokens — terminal escuro: preto, branco, verde forte e laranja fraco só para alerta
   (60-30-10), como o site do fuckads. Só tema escuro.
   ========================================================================= */
:root {
  color-scheme: dark;
  --bg: #0a0b09;
  --card: #0e100c;
  --sunken: #070806;
  --border: #1d211a;
  --border-strong: #2e3328;
  --text: #edefe8;
  --muted: #8a9182;
  --faint: #7a8272;
  --accent: #b7ff2e;
  --accent-hover: #ccff5e;
  --accent-soft: rgba(183, 255, 46, 0.07);
  --on-accent: #0c0e08;
  /* sucesso é o próprio verde; falha e aviso são o laranja */
  --ok: var(--accent);
  --ok-bg: var(--accent-soft);
  --alert: #e9a06b;
  --alert-soft: rgba(233, 160, 107, 0.08);
  --err: var(--alert);
  --err-bg: var(--alert-soft);
  --warn: var(--alert);
  --warn-bg: var(--alert-soft);
  --info: var(--text);
  --info-bg: rgba(237, 239, 232, 0.05);
  --off-bg: rgba(237, 239, 232, 0.04);
  --sel: rgba(183, 255, 46, 0.05);
  /* trecho alterado dentro da linha, no diff das sugestões */
  --diff-add-hl: rgba(183, 255, 46, 0.22);
  --diff-del-hl: rgba(233, 160, 107, 0.26);
  --nav-bg: rgba(10, 11, 9, 0.92);
  --grid: rgba(237, 239, 232, 0.028);
  --noise: 0.035;
  --mono: "JetBrains Mono Variable", "JetBrains Mono", ui-monospace, "DejaVu Sans Mono", monospace;
  --sans: var(--mono);
  /* canto cortado (cartões e painéis) e marcador verde nos cantos de cima */
  --cut: 14px;
  --cut-sm: 7px;
  --radius: 0;
  --radius-lg: 0;
  font-family: var(--mono);
  font-size: 13px;
  line-height: 1.55;
  color: var(--text);
  background: var(--bg);
  -webkit-font-smoothing: antialiased;
  scrollbar-color: var(--border-strong) transparent;
}
* {
  box-sizing: border-box;
}
/* grade fina fixa e ruído por cima de tudo, sem animação */
body {
  margin: 0;
  background-color: var(--bg);
  background-image:
    linear-gradient(var(--grid) 1px, transparent 1px),
    linear-gradient(90deg, var(--grid) 1px, transparent 1px);
  background-size: 48px 48px;
  background-attachment: fixed;
}
body::after {
  content: "";
  position: fixed;
  inset: 0;
  z-index: 200;
  pointer-events: none;
  opacity: var(--noise);
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='160' height='160'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='2' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='160' height='160' filter='url(%23n)'/%3E%3C/svg%3E");
}
::selection {
  background: var(--accent);
  color: var(--on-accent);
}
/* moldura: canto inferior direito cortado e cantos de cima marcados em verde */
.card,
.panel,
.finding {
  position: relative;
  clip-path: polygon(0 0, 100% 0, 100% calc(100% - var(--cut)), calc(100% - var(--cut)) 100%, 0 100%);
}
.card::before,
.card::after,
.panel::before,
.panel::after {
  content: "";
  position: absolute;
  top: 0;
  width: 9px;
  height: 9px;
  pointer-events: none;
  border-top: 1px solid var(--accent);
  z-index: 2;
}
.card::before,
.panel::before {
  left: 0;
  border-left: 1px solid var(--accent);
}
.card::after,
.panel::after {
  right: 0;
  border-right: 1px solid var(--accent);
}
.finding {
  --cut: 9px;
}

/* =========================================================================
   Estrutura
   ========================================================================= */
header.top {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 24px;
  padding: 12px 20px;
  border-bottom: 1px solid var(--border);
  background: var(--nav-bg);
  position: sticky;
  top: 0;
  z-index: 5;
}
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
}
.brand-mark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 22px;
  background: var(--accent);
  color: var(--on-accent);
  font-size: 0.72rem;
  font-weight: 800;
  letter-spacing: 0.04em;
  clip-path: polygon(0 0, 100% 0, 100% calc(100% - 5px), calc(100% - 5px) 100%, 0 100%);
}
h1 {
  font-weight: 700;
  font-size: 0.95rem;
  letter-spacing: 0.14em;
  text-transform: uppercase;
  margin: 0;
}
h2 {
  font-weight: 700;
  font-size: 1rem;
  letter-spacing: -0.01em;
  margin: 0 0 12px;
}
h3 {
  font-size: 0.74rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.16em;
  color: var(--muted);
  margin: 22px 0 10px;
}
main {
  padding: 20px;
}
.narrow {
  max-width: 780px;
  margin: 0 auto;
}

/* abas sublinhadas */
.tabs {
  display: flex;
  gap: 4px;
  align-self: stretch;
}
button.tab {
  background: transparent;
  color: var(--muted);
  border: none;
  border-bottom: 2px solid transparent;
  border-radius: 0;
  padding: 0 12px;
  margin-bottom: -13px;
  font-size: 0.74rem;
  letter-spacing: 0.14em;
  text-transform: uppercase;
  clip-path: none;
}
button.tab:hover:not(:disabled) {
  color: var(--text);
  background: transparent;
  border-color: transparent;
}
button.tab.active {
  color: var(--accent);
  border-bottom-color: var(--accent);
}

/* aviso de limite de uso */
.limit-banner {
  display: flex;
  align-items: flex-start;
  gap: 12px;
  margin: 16px 20px 0;
  padding: 12px 14px;
  background: var(--alert-soft);
  border: 1px solid color-mix(in srgb, var(--alert) 40%, transparent);
  border-left: 3px solid var(--alert);
}
.limit-icon {
  color: var(--alert);
  font-size: 1.1rem;
  line-height: 1.3;
}
.limit-text {
  flex: 1;
}
.limit-msg {
  font-family: var(--mono);
  font-size: 0.75rem;
  color: var(--muted);
  margin-top: 4px;
}

/* =========================================================================
   Componentes base
   ========================================================================= */
.card {
  background: var(--card);
  border: 1px solid var(--border);
  padding: 18px 20px;
  margin-bottom: 16px;
}
button {
  font: inherit;
  font-size: 0.74rem;
  font-weight: 700;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  padding: 8px 14px;
  border-radius: 0;
  border: 1px solid var(--accent);
  background: var(--accent);
  color: var(--on-accent);
  cursor: pointer;
  white-space: nowrap;
  clip-path: polygon(0 0, 100% 0, 100% calc(100% - var(--cut-sm)), calc(100% - var(--cut-sm)) 100%, 0 100%);
  transition: background 0.15s, border-color 0.15s, color 0.15s, opacity 0.15s;
}
button:hover:not(:disabled) {
  background: var(--accent-hover);
  border-color: var(--accent-hover);
}
button.secondary {
  background: transparent;
  color: var(--text);
  border-color: var(--border-strong);
}
button.secondary:hover:not(:disabled) {
  background: transparent;
  border-color: var(--accent);
  color: var(--accent);
}
button.link {
  background: none;
  border: none;
  color: var(--accent);
  padding: 0;
  font-weight: 500;
  font-size: inherit;
  letter-spacing: 0;
  text-transform: none;
  clip-path: none;
}
button.link:hover:not(:disabled) {
  background: none;
  color: var(--accent-hover);
}
button:disabled {
  opacity: 0.4;
  cursor: default;
}
/* o canto cortado (clip-path) esconderia o contorno de fora: o foco fica por dentro */
button:focus-visible,
input:focus-visible,
textarea:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: -3px;
}
/* só nos botões com fundo verde o contorno escuro aparece; nos sem fundo ele fica verde */
button:not(.secondary):not(.link):not(.tab):not(.icon-btn):not(.tab-close):not(.chip-btn):not(.tile):focus-visible {
  outline-color: var(--on-accent);
}
button.small-btn {
  padding: 4px 10px;
  font-size: 0.68rem;
  --cut-sm: 5px;
}
a {
  color: var(--accent);
  text-decoration: none;
}
a:hover {
  color: var(--accent-hover);
  text-decoration: underline;
  text-underline-offset: 2px;
}
code {
  font-family: var(--mono);
  font-size: 0.82em;
}
form {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  align-items: end;
}
form.grid > label {
  flex: 1 1 260px;
}
form.grid > label.wide {
  flex-basis: 100%;
}
label {
  display: flex;
  flex-direction: column;
  gap: 5px;
  font-size: 0.72rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--muted);
}
input {
  font: inherit;
  font-size: 0.92rem;
  letter-spacing: 0;
  text-transform: none;
  padding: 8px 11px;
  border: 1px solid var(--border-strong);
  border-radius: 0;
  background: var(--sunken);
  color: var(--text);
  min-width: 0;
  transition: border-color 0.15s;
}
input:focus {
  border-color: var(--accent);
}
label.check {
  flex-direction: row;
  align-items: center;
  gap: 6px;
  font-weight: normal;
  font-family: var(--sans);
  font-size: 0.82rem;
  text-transform: none;
  letter-spacing: 0;
}
input[type="checkbox"] {
  accent-color: var(--accent);
}

/* selos */
.badge {
  display: inline-block;
  font-size: 0.66rem;
  font-weight: 600;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  padding: 1px 7px;
  margin-left: 4px;
  border: 1px solid var(--border-strong);
  background: transparent;
  color: var(--muted);
  vertical-align: 1px;
}
.badge.ok,
.badge.completed,
.badge.running {
  border-color: color-mix(in srgb, var(--accent) 45%, transparent);
}
.badge.error,
.badge.failed,
.badge.warn,
.badge.blocked {
  border-color: color-mix(in srgb, var(--alert) 45%, transparent);
}
.badge.ok,
.badge.completed {
  background: var(--ok-bg);
  color: var(--ok);
}
.badge.error,
.badge.failed {
  background: var(--err-bg);
  color: var(--err);
}
.badge.warn,
.badge.blocked {
  background: var(--warn-bg);
  color: var(--warn);
}
.badge.running {
  background: var(--accent-soft);
  color: var(--accent);
}

/* mensagens */
.msg {
  margin: 10px 0;
  padding: 10px 12px;
  border-left: 3px solid currentColor;
  white-space: pre-wrap;
}
.msg.ok {
  background: var(--ok-bg);
  color: var(--ok);
}
.msg.error {
  background: var(--err-bg);
  color: var(--err);
}
.msg.warn {
  background: var(--warn-bg);
  color: var(--warn);
}
.msg.warn .alert-actions button {
  background: var(--warn-bg);
  color: var(--warn);
  border: 1px solid var(--warn);
}
.msg.warn .alert-actions button:hover:not(:disabled) {
  background: var(--warn);
  border-color: var(--warn);
  color: var(--on-accent);
}
.msg.warn .alert-actions button:focus-visible {
  outline-color: var(--alert);
}
.alert-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 8px;
  flex-wrap: wrap;
}
.muted {
  color: var(--muted);
}
.small {
  font-size: 0.82rem;
}
dl {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 8px 18px;
  margin: 0;
}
dt {
  color: var(--muted);
}
dd {
  margin: 0;
  overflow-wrap: anywhere;
}

/* =========================================================================
   Configurações
   ========================================================================= */
.token {
  margin-top: 16px;
  padding-top: 16px;
  border-top: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.token input {
  flex: 1 1 260px;
}
form.test-app {
  border-top: 1px solid var(--border);
  padding-top: 12px;
  margin-bottom: 14px;
}
.wide-row {
  flex-basis: 100%;
  margin: 0;
  text-transform: none;
  letter-spacing: 0;
  color: var(--text);
  font-weight: 600;
  font-size: 0.92rem;
}

/* =========================================================================
   Lista de PRs
   ========================================================================= */
.prs-layout {
  display: grid;
  grid-template-columns: 1fr;
  gap: 18px;
}
.prs-layout.with-panel {
  grid-template-columns: minmax(0, 1fr) minmax(400px, 46%);
}
/* painel expandido: ocupa a largura toda e a lista fica escondida (continua montada, com o filtro) */
.prs-layout.with-panel:has(> .panel.expanded) {
  grid-template-columns: minmax(0, 1fr);
}
.prs-layout:has(> .panel.expanded) > .prs {
  display: none;
}
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 14px;
}
h2.section-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
}
table {
  width: 100%;
  border-collapse: collapse;
}
tr {
  border-top: 1px solid var(--border);
}
tr:first-child {
  border-top: none;
}
tr.selected {
  background: var(--sel);
}
tr.clickable {
  cursor: pointer;
}
tr.clickable:hover:not(.selected) {
  background: var(--sunken);
}
td {
  padding: 11px 8px;
  vertical-align: top;
}
td.pr-main .title {
  margin-left: 6px;
  font-weight: 500;
}
td.pr-action {
  text-align: right;
  white-space: nowrap;
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  gap: 6px;
}
.actions {
  display: flex;
  gap: 6px;
}
.reviewers {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  align-items: center;
  margin-top: 5px;
}
.chip {
  font-size: 0.7rem;
  padding: 0 6px;
  border: 1px solid var(--border-strong);
  color: var(--muted);
}
.chip.ap {
  color: var(--ok);
  border-color: color-mix(in srgb, var(--ok) 45%, transparent);
}
.chip.cr {
  color: var(--err);
  border-color: color-mix(in srgb, var(--err) 45%, transparent);
}

/* =========================================================================
   Painel do review
   ========================================================================= */
.panel {
  background: var(--card);
  border: 1px solid var(--border);
  align-self: start;
  position: sticky;
  top: 72px;
  height: calc(100vh - 92px);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.panel-scroll {
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 4px 18px 28px;
}
.panel-head {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: start;
  padding: 18px 18px 8px;
}
/* abas do painel: Review | Conversa */
.panel-tabs {
  display: flex;
  gap: 4px;
  padding: 0 18px;
  border-bottom: 1px solid var(--border);
}
.panel-tabs button.tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 6px 12px 8px;
  margin-bottom: -1px;
}
.tab-count {
  font-size: 0.68rem;
  line-height: 1;
  padding: 2px 6px;
  border: 1px solid var(--border-strong);
  color: var(--muted);
  letter-spacing: 0;
}
.panel-error {
  margin: 10px 18px 0;
}
.panel-head h2 {
  margin-bottom: 2px;
}
.status-line {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
  margin: 12px 0;
}
h3.with-action {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 0;
}
h3.sub {
  margin-top: 2px;
  font-weight: normal;
  text-transform: none;
  letter-spacing: 0;
}
.finding {
  border: 1px solid var(--border);
  padding: 12px 14px;
  margin-bottom: 10px;
  background: var(--card);
}
.finding p {
  margin: 6px 0;
}
.finding-head {
  display: flex;
  gap: 8px;
  align-items: baseline;
}
.push {
  margin-left: auto;
}
.fid {
  font-weight: 600;
  color: var(--text);
}
/* item do pedido de mudanças: checkbox + rótulo, comentário e o porquê */
.rc-item {
  border-top: 1px solid var(--border);
  padding: 8px 0 4px;
}
.rc-item p {
  margin: 4px 0;
}
/* severidade: laranja só em CRITICAL e HIGH; MEDIUM em branco; o resto em cinza */
.sev,
.tstat,
.imp {
  font-size: 0.64rem;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.12em;
  padding: 1px 7px;
  border: 1px solid var(--border-strong);
  background: transparent;
  color: var(--muted);
  white-space: nowrap;
}
.sev.critical,
.sev.high,
.tstat.reprovado {
  background: var(--alert-soft);
  border-color: color-mix(in srgb, var(--alert) 55%, transparent);
  color: var(--alert);
}
.sev.critical {
  background: var(--alert);
  border-color: var(--alert);
  color: var(--on-accent);
}
.sev.medium,
.tstat.bloqueado,
.sev.melhoria,
.imp.media {
  border-color: color-mix(in srgb, var(--text) 35%, transparent);
  color: var(--text);
}
.imp.alta {
  border-color: color-mix(in srgb, var(--accent) 55%, transparent);
  color: var(--accent);
}
.suggestion {
  background: var(--sunken);
  border: 1px solid var(--border);
  padding: 8px 10px;
  margin: 4px 0 0;
  overflow: auto;
  font-size: 0.78rem;
  white-space: pre;
}
/* =========================================================================
   Meus PRs — comentários, sugestão em diff e commit
   ========================================================================= */
/* seletor Review | Correções do painel da branch local, logo abaixo do cabeçalho */
.mode-switch {
  display: flex;
  gap: 6px;
  padding: 0 18px 10px;
}
.mode-switch button.on {
  background: var(--accent-soft);
  color: var(--accent);
  border-color: var(--accent);
}
.head-actions {
  display: inline-flex;
  gap: 6px;
}
.toolbar input.filter {
  width: 260px;
}
.hidden-list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.hidden-list li {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 6px 0;
  border-top: 1px solid var(--border);
}
.hidden-list li:first-child {
  border-top: 0;
}
.pager {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
}
details.project {
  padding: 0;
}
details.project > summary {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 12px 16px;
  cursor: pointer;
  list-style: none;
}
details.project > summary::-webkit-details-marker {
  display: none;
}
details.project > summary::before {
  content: "▸";
  color: var(--muted);
  transition: transform 0.15s;
}
details.project[open] > summary::before {
  transform: rotate(90deg);
}
details.project > .project-body {
  padding: 0 16px 14px;
}
.comment-body {
  margin: 8px 0;
  padding: 2px 0 2px 10px;
  border-left: 3px solid var(--border-strong);
}
.reply {
  margin: 4px 0 4px 13px;
  color: var(--muted);
  white-space: pre-wrap;
}
.explanation {
  white-space: pre-wrap;
}
.commit-box {
  margin: 14px 0 10px;
  padding-top: 12px;
  border-top: 1px solid var(--border);
}
.diff {
  border: 1px solid var(--border);
  margin: 8px 0 0;
  overflow: hidden;
  font-family: var(--mono);
  font-size: 0.78rem;
}
.diff-head {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 5px 10px;
  background: var(--sunken);
  border-bottom: 1px solid var(--border);
  font-family: var(--sans);
}
.diff-head .badge {
  margin-left: auto;
}
/* uma coluna da largura da linha mais longa (no mínimo a da caixa): com rolagem lateral, o fundo
   de cada linha acompanha o texto até o fim, e não só a parte visível */
.diff-body {
  overflow-x: auto;
  display: grid;
  grid-template-columns: minmax(100%, max-content);
}
.diff-line {
  display: grid;
  grid-template-columns: 3.2em 3.2em 1.4em max-content;
  line-height: 1.55;
}
.diff-line .ln {
  padding: 0 6px;
  text-align: right;
  color: var(--muted);
  user-select: none;
  border-right: 1px solid var(--border);
}
.diff-line .sign {
  text-align: center;
  user-select: none;
  color: var(--muted);
}
.diff-line .code {
  white-space: pre;
  padding-right: 12px;
}
.diff-line.del {
  background: var(--err-bg);
}
.diff-line.add {
  background: var(--ok-bg);
}
.diff-line.del .sign {
  color: var(--err);
}
.diff-line.add .sign {
  color: var(--ok);
}
.diff-line.del .hl {
  background: var(--diff-del-hl);
}
.diff-line.add .hl {
  background: var(--diff-add-hl);
}
.tstat.aprovado {
  border-color: color-mix(in srgb, var(--accent) 55%, transparent);
  color: var(--accent);
}
.steps {
  margin: 6px 0;
  padding-left: 20px;
}
.audit {
  font-size: 0.84rem;
  padding: 7px 10px;
  background: var(--off-bg);
  border-left: 3px solid var(--border-strong);
}
.audit.confirmado {
  border-left-color: var(--accent);
}
.audit.evidencia_insuficiente {
  border-left-color: var(--alert);
}
.audit.confirmado {
  background: var(--ok-bg);
}
.audit.evidencia_insuficiente {
  background: var(--warn-bg);
}
.discarded {
  margin: 8px 0;
}
.log-section {
  margin-top: 18px;
}
.log-section summary {
  cursor: pointer;
  color: var(--muted);
  font-size: 0.82rem;
  margin-bottom: 6px;
}
.log {
  background: var(--sunken);
  border: 1px solid var(--border);
  padding: 10px;
  font-family: var(--mono);
  font-size: 0.72rem;
  max-height: 260px;
  overflow: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  margin: 0;
}

/* gatilhos: o log ocupa a altura do painel */
.log.log-fill {
  max-height: none;
  flex: 1;
  min-height: 0;
}
.panel-scroll.trigger-scroll {
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow: hidden;
}
form.grid textarea {
  font: inherit;
  font-size: 0.92rem;
  letter-spacing: 0;
  text-transform: none;
  padding: 8px 11px;
  border: 1px solid var(--border-strong);
  border-radius: 0;
  background: var(--sunken);
  color: var(--text);
  resize: vertical;
}
.mode-switch.inline {
  padding-left: 0;
  padding-right: 0;
}
form.grid > .field {
  flex: 1 1 260px;
}
form.grid > .wide-row {
  flex-basis: 100%;
}
.field {
  display: flex;
  flex-direction: column;
  gap: 5px;
}
.field-label {
  font-size: 0.72rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--muted);
}

/* ---- conversa (aba com a altura toda do painel) ---- */
.chat-pane {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.chat-scroll {
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 18px 18px 32px;
}

/* ---- correções: uma aba por conversa; a barra rola na horizontal e a aba da lista fica fixa ---- */
.panel-tabs.tabs-scroll {
  position: relative;
  padding: 0 18px 0 0;
  /* com rolagem, a aba não pode descer 1 px sobre a borda (abriria rolagem vertical): a linha vira sombra */
  border-bottom: none;
  box-shadow: inset 0 -1px 0 var(--border);
  overflow-x: auto;
  overflow-y: hidden;
  scrollbar-width: thin;
}
.tabs-scroll button.tab {
  margin-bottom: 0;
}
.tab-pin {
  position: sticky;
  left: 0;
  z-index: 1;
  display: inline-flex;
  flex-shrink: 0;
  padding-left: 18px;
  background: var(--card);
  box-shadow: inset 0 -1px 0 var(--border);
}
.chat-tab {
  display: inline-flex;
  align-items: center;
  flex-shrink: 0;
  border-bottom: 2px solid transparent;
}
.chat-tab.active {
  border-bottom-color: var(--accent);
}
.tabs-scroll .chat-tab button.tab {
  border-bottom: none;
  padding-right: 4px;
}
.tabs-scroll .chat-tab.active button.tab {
  color: var(--text);
}
button.tab-close {
  background: transparent;
  border: none;
  color: var(--muted);
  padding: 2px 6px;
  line-height: 1;
  clip-path: none;
}
button.tab-close:hover:not(:disabled) {
  background: var(--sunken);
  color: var(--text);
}
.chat-empty {
  max-width: 560px;
  margin: 12vh auto 0;
  text-align: center;
}
.chat-empty .chips {
  justify-content: center;
}
.thread {
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.msg-user {
  align-self: stretch;
  background: var(--sunken);
  padding: 8px 12px;
  border-left: 3px solid var(--border-strong);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.msg-claude {
  align-self: stretch;
  padding-left: 12px;
  border-left: 3px solid var(--accent);
}
.msg-claude-head {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 0.68rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.16em;
  color: var(--muted);
  margin-bottom: 4px;
}
.spinner {
  display: inline-block;
  width: 10px;
  height: 10px;
  border: 2px solid var(--border-strong);
  border-top-color: var(--accent);
  animation: spin 0.8s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
.dots::after {
  content: "";
  animation: dots 1.4s steps(4, end) infinite;
}
@keyframes dots {
  0% { content: ""; }
  25% { content: "."; }
  50% { content: ".."; }
  75% { content: "..."; }
}
.msg-actions {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 0.75rem;
  margin-top: 4px;
  opacity: 0.55;
  transition: opacity 0.15s;
}
.msg-claude:hover .msg-actions {
  opacity: 1;
}
.icon-btn {
  background: none;
  border: none;
  color: var(--muted);
  padding: 2px 4px;
  font-size: 0.68rem;
  font-weight: normal;
  clip-path: none;
}
.icon-btn:hover:not(:disabled) {
  background: none;
  color: var(--text);
}
.msg-error {
  color: var(--err);
  font-size: 0.85rem;
  padding: 6px 10px;
  border-left: 3px solid var(--err);
}
.steps-box {
  font-size: 0.78rem;
  color: var(--muted);
}
.steps-box summary {
  cursor: pointer;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.step-line {
  font-family: var(--mono);
  font-size: 0.7rem;
  padding: 1px 0 1px 14px;
  overflow-wrap: anywhere;
}
/* markdown das respostas */
.md {
  line-height: 1.6;
  overflow-wrap: anywhere;
}
.md > :first-child {
  margin-top: 0;
}
.md > :last-child {
  margin-bottom: 0;
}
.md p,
.md ul,
.md ol {
  margin: 0 0 8px;
}
.md ul,
.md ol {
  padding-left: 22px;
}
.md h1,
.md h2,
.md h3,
.md h4 {
  font-weight: 600;
  font-size: 1rem;
  text-transform: none;
  letter-spacing: 0;
  color: var(--text);
  margin: 14px 0 6px;
}
.md code {
  background: var(--sunken);
  color: var(--accent);
  padding: 1px 5px;
}
.md pre {
  background: var(--sunken);
  border: 1px solid var(--border);
  padding: 10px 12px;
  overflow: auto;
  margin: 0 0 8px;
}
.md pre code {
  background: none;
  color: var(--text);
  padding: 0;
  font-size: 0.78rem;
}
.md blockquote {
  margin: 0 0 8px;
  padding-left: 10px;
  border-left: 3px solid var(--border-strong);
  color: var(--muted);
}
.md table {
  border-collapse: collapse;
  margin: 0 0 8px;
  font-size: 0.85rem;
}
.md th,
.md td {
  border: 1px solid var(--border);
  padding: 4px 8px;
}
.md a {
  text-decoration: underline;
  text-underline-offset: 2px;
}

/* ---- campo de pergunta flutuante ---- */
.composer-dock {
  padding: 0 18px 18px;
  background: linear-gradient(to bottom, transparent, var(--card) 32%);
  margin-top: -20px;
  position: relative;
  z-index: 1;
}
.chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 8px;
}
.chip-btn {
  background: transparent;
  color: var(--text);
  border: 1px solid var(--border-strong);
  padding: 4px 10px;
  font-size: 0.74rem;
  font-weight: normal;
  letter-spacing: 0;
  text-transform: none;
  --cut-sm: 5px;
}
.chip-btn:hover:not(:disabled) {
  background: var(--card);
  border-color: var(--accent);
  color: var(--accent);
}
/* é um <form>: desfaz o `form { flex-wrap: wrap; align-items: end }` global, que deixava o
   textarea com a largura padrão do navegador, encostado à direita */
.composer {
  display: flex;
  flex-direction: column;
  flex-wrap: nowrap;
  align-items: stretch;
  gap: 6px;
  cursor: text;
  background: var(--card);
  border: 1px solid var(--border-strong);
  padding: 10px 10px 8px 12px;
  transition: border-color 0.15s;
}
.composer:focus-within {
  border-color: var(--accent);
}
/* overflow: com zoom acima de 100%, o WebKitGTK mede o placeholder com o zoom aplicado duas vezes
   e mostrava barras de rolagem no campo vazio. O texto quebra linha, então nunca rola na
   horizontal; a rolagem vertical só liga no autosize, quando passa da altura máxima. */
.composer textarea {
  font: inherit;
  width: 100%;
  overflow: hidden;
  border: none;
  outline: none;
  resize: none;
  background: transparent;
  color: var(--text);
  padding: 2px 0;
  min-height: 66px;
  max-height: 40vh;
  line-height: 1.45;
}
.composer textarea::placeholder {
  color: var(--muted);
}
.composer-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.model-chip {
  font-size: 0.72rem;
  color: var(--muted);
}
.send-btn {
  width: 30px;
  height: 30px;
  padding: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: none;
  background: var(--accent);
  color: var(--on-accent);
}
.send-btn:hover:not(:disabled) {
  background: var(--accent-hover);
}
.send-btn:disabled {
  opacity: 0.35;
}
.send-btn.stop,
.send-btn.stop:hover:not(:disabled) {
  background: var(--alert);
  color: var(--on-accent);
}
/* =========================================================================
   Painel de abertura (Para eu revisar): os números do dia; cada bloco filtra a lista
   ========================================================================= */
.mission {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: 1px;
  background: var(--border);
  border: 1px solid var(--border);
  margin-bottom: 8px;
}
.mission button.tile {
  display: grid;
  gap: 6px;
  align-content: start;
  text-align: left;
  padding: 14px 16px 12px;
  background: var(--sunken);
  border: none;
  color: var(--text);
  clip-path: none;
  font-weight: 400;
  letter-spacing: 0;
  text-transform: none;
  white-space: normal;
}
.mission button.tile:hover:not(:disabled) {
  background: var(--accent-soft);
  color: var(--text);
}
.mission button.tile.on {
  background: var(--accent-soft);
  box-shadow: inset 0 -2px 0 var(--accent);
}
.tile-label {
  font-size: 0.68rem;
  font-weight: 500;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--muted);
}
.tile-n {
  font-size: 2.9rem;
  line-height: 0.95;
  font-weight: 800;
  letter-spacing: -0.03em;
  font-variant-numeric: tabular-nums;
}
.tile.hot .tile-n {
  color: var(--accent);
}
.tile.alert .tile-n {
  color: var(--alert);
}
.tile.zero .tile-n {
  color: var(--faint);
}
.tile-sub {
  font-size: 0.74rem;
  color: var(--muted);
}
.mission-strip {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 20px;
  margin: 0 0 16px;
  font-size: 0.78rem;
  color: var(--muted);
}
.mission-strip b {
  color: var(--text);
}

/* =========================================================================
   Pipeline do review e etapas como log de terminal
   ========================================================================= */
.pipe {
  display: flex;
  align-items: stretch;
  overflow-x: auto;
  margin: 4px 0 14px;
  padding-bottom: 2px;
}
.pipe-node {
  flex: 1 0 92px;
  display: grid;
  gap: 2px;
  padding: 8px 10px;
  border: 1px solid var(--border-strong);
  border-style: dashed;
  background: var(--sunken);
  color: var(--faint);
}
.pipe-name {
  font-size: 0.7rem;
  font-weight: 700;
  letter-spacing: 0.12em;
  text-transform: uppercase;
}
.pipe-detail {
  font-size: 0.7rem;
}
.pipe-node.on,
.pipe-node.run {
  border-style: solid;
  border-color: var(--accent);
  color: var(--text);
}
.pipe-node.on .pipe-name,
.pipe-node.run .pipe-name {
  color: var(--accent);
}
.pipe-node.run {
  background: var(--accent-soft);
}
.pipe-node.fail {
  border-style: solid;
  border-color: var(--alert);
  background: var(--alert-soft);
  color: var(--text);
}
.pipe-node.fail .pipe-name {
  color: var(--alert);
}
.pipe-link {
  flex: 0 0 16px;
  align-self: center;
  height: 1px;
  background: var(--border-strong);
}
.pipe-link.on {
  background: var(--accent);
}
.term {
  background: var(--sunken);
  border: 1px solid var(--border);
  overflow-x: auto;
  margin-bottom: 12px;
}
.term-bar {
  display: flex;
  gap: 14px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--border);
  font-size: 0.68rem;
  color: var(--faint);
  white-space: nowrap;
}
.term-lines {
  list-style: none;
  margin: 0;
  padding: 8px 10px;
  font-size: 0.74rem;
  line-height: 1.75;
  min-width: max-content;
}
.term-line {
  display: grid;
  grid-template-columns: 6.2em minmax(16em, auto) 6.5em 6em 7em;
  column-gap: 10px;
  white-space: nowrap;
}
.term-time {
  color: var(--faint);
}
.term-label {
  color: var(--text);
  font-weight: 600;
}
.term-took,
.term-cost {
  color: var(--muted);
  text-align: right;
}
.term-state {
  color: var(--accent);
}
.term-line.run .term-state {
  color: var(--text);
}
.term-line.fail .term-state,
.term-line.cut .term-state,
.term-call.fail {
  color: var(--alert);
}
.term-call {
  grid-column: 2 / -1;
  color: var(--muted);
  font-weight: 400;
}
.term-line.loose .term-label {
  font-weight: 400;
  color: var(--muted);
}
.term-total .term-time,
.term-total .term-label {
  color: var(--accent);
}
.term-total.fail .term-time,
.term-total.fail .term-label {
  color: var(--alert);
}

/* =========================================================================
   Achados como alvos: carimbo do Opus, falso positivo riscado, nota do juiz em barra
   ========================================================================= */
.verdict {
  display: flex;
  align-items: baseline;
  gap: 10px;
  flex-wrap: wrap;
  margin-top: 8px;
}
.stamp {
  display: inline-block;
  font-size: 0.68rem;
  font-weight: 800;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  padding: 2px 9px;
  border: 2px solid currentColor;
  transform: rotate(-2deg);
  white-space: nowrap;
}
.stamp.ok {
  color: var(--accent);
}
.stamp.al {
  color: var(--alert);
}
.stamp.mid {
  color: var(--text);
}
.finding.fp {
  opacity: 0.85;
}
.fp-title {
  color: var(--muted);
  text-decoration: line-through;
  text-decoration-color: var(--alert);
}
.score {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 8px 0 2px;
  font-size: 0.78rem;
}
.score-bar {
  display: grid;
  grid-template-columns: repeat(10, 12px);
  gap: 3px;
}
.score-bar i {
  height: 8px;
  background: var(--border-strong);
}
.score-bar i.f {
  background: var(--accent);
}

/* =========================================================================
   Diff lado a lado
   ========================================================================= */
.diff-mode {
  margin-left: auto;
  display: inline-flex;
  gap: 2px;
}
.diff-head .badge + .diff-mode {
  margin-left: 8px;
}
.diff-mode .icon-btn.on {
  color: var(--accent);
}
.diff-split {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
}
.diff-side {
  min-width: 0;
}
.diff-side + .diff-side {
  border-left: 1px solid var(--border);
}
.diff-side-head {
  padding: 3px 10px;
  font-size: 0.64rem;
  letter-spacing: 0.14em;
  text-transform: uppercase;
  border-bottom: 1px solid var(--border);
}
.del-side .diff-side-head {
  color: var(--alert);
}
.add-side .diff-side-head {
  color: var(--accent);
}
.diff-line.one {
  grid-template-columns: 3.2em 1.4em max-content;
  /* a linha vazia do lado que não tem par precisa da mesma altura, senão os lados desencontram */
  min-height: 1.55em;
}
.diff-line.gap {
  background: repeating-linear-gradient(135deg, transparent 0 6px, var(--off-bg) 6px 7px);
}
/* separador `@@ … @@` entre trechos do diff do PR */
.diff-line.hunk {
  display: block;
  background: var(--sunken);
  border-top: 1px solid var(--border);
  border-bottom: 1px solid var(--border);
}
.hunk-text {
  display: block;
  padding: 0 10px;
  white-space: pre;
  color: var(--muted);
}
/* aba Diff do painel do review */
.pr-diff-bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
  padding: 10px 0 8px;
}
.pr-diff-bar .mode-switch {
  padding: 0;
}
.pr-diff-files {
  margin: 4px 0 14px;
}
.pr-diff-files > summary {
  cursor: pointer;
  color: var(--muted);
  font-size: 0.82rem;
}
.pr-diff-index {
  list-style: none;
  margin: 6px 0 0;
  padding: 8px 10px;
  border: 1px solid var(--border);
  background: var(--sunken);
  font-size: 0.78rem;
}
.pr-diff-index li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}
.pr-diff-index .icon-btn {
  text-align: left;
}
.pr-diff-file {
  margin: 0 0 14px;
}
.pr-diff-file .diff-head {
  gap: 6px;
}
.pr-diff-file .diff-head .badge {
  margin-left: 0;
}
.pr-diff-file .file-stat {
  margin-left: auto;
}
.pr-diff-note {
  margin: 0;
  padding: 8px 10px;
}
.outside-marks {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 14px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--border);
}
.outside-mark .diff-mark {
  margin: 0 6px 0 0;
}
/* achado aberto a partir do diff: borda verde por 1,5 s, sem piscar. Por dentro da caixa:
   o clip-path do .finding corta um outline de fora. */
.finding.flash {
  box-shadow: inset 0 0 0 1px var(--accent);
}
/* selo de um achado na linha do diff: leva ao achado na aba Review */
.sev.diff-mark {
  margin-left: 12px;
  cursor: pointer;
  vertical-align: 1px;
}

/* =========================================================================
   Uso: custo por review e por agente
   ========================================================================= */
.narrow.wide {
  max-width: 980px;
}
.kpis {
  display: flex;
  flex-wrap: wrap;
  gap: 12px 32px;
  margin: 4px 0 8px;
}
.kpi {
  display: grid;
  gap: 2px;
}
.kpi b {
  font-size: 1.7rem;
  font-weight: 800;
  letter-spacing: -0.02em;
  font-variant-numeric: tabular-nums;
}
.kpi b.on {
  color: var(--accent);
}
.chart-wrap {
  overflow-x: auto;
  margin: 8px 0;
}
.chart {
  width: 100%;
  min-width: 520px;
  height: auto;
  display: block;
}
.chart text {
  font-family: var(--mono);
}
.chart .grid-line {
  stroke: var(--border);
  stroke-width: 1;
}
.chart .axis {
  font-size: 10.5px;
  fill: var(--faint);
}
.chart .axis.on {
  fill: var(--accent);
}
.chart .axis.al {
  fill: var(--alert);
}
.chart .label {
  font-size: 12px;
  fill: var(--text);
}
.chart .value {
  font-size: 12px;
  font-weight: 700;
  fill: var(--text);
}
.chart .bar,
.chart .col {
  fill: var(--accent);
}
.chart .col.high {
  fill: var(--alert);
}
.chart .avg-line {
  stroke: var(--accent);
  stroke-width: 1;
  stroke-dasharray: 4 4;
}
.usage-table {
  margin-top: 10px;
  font-size: 0.8rem;
  font-variant-numeric: tabular-nums;
}
.usage-table th {
  text-align: left;
  font-size: 0.66rem;
  font-weight: 600;
  letter-spacing: 0.14em;
  text-transform: uppercase;
  color: var(--muted);
  padding: 6px 8px;
}
.usage-table td {
  padding: 6px 8px;
}
.usage-table td.alert {
  color: var(--alert);
}
</style>
