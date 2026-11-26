<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { api, errorText, events, type Trigger, type TriggerState, type TriggerView } from "../api";
import TriggerForm from "./TriggerForm.vue";

const MAX_LINES = 5000;
const views = ref<TriggerView[]>([]);
const openId = ref<string | null>(null);
const editing = ref<Trigger | null>(null);
const error = ref("");
/** Gatilhos com Rodar, Parar ou Reiniciar em andamento: uma parada lenta não trava as outras linhas. */
const busy = ref<Record<string, boolean>>({});
const confirmDelete = ref(false);
const copied = ref(false);
const log = ref<string[]>([]);
const logBox = ref<HTMLElement | null>(null);
const unlisten: (() => void)[] = [];
/** Enquanto o fim do log carrega, os eventos do gatilho aberto ficam aqui para não se perderem. */
let loadingLog: string | null = null;
let pending: string[] = [];

const open = computed(() => views.value.find((v) => v.trigger.id === openId.value) ?? null);
const showPanel = computed(() => !!open.value || !!editing.value);

function newTrigger(): Trigger {
  return {
    id: "",
    name: "",
    kind: "npm",
    package_json: "",
    command: "run",
    scripts: [],
    package_manager: "npm",
    node: "",
    node_options: "",
    workdir: "",
    script: "",
    stop_script: "",
    args: "",
    env: [],
  };
}

async function load() {
  try {
    views.value = await api.triggersList();
  } catch (e) {
    error.value = errorText(e);
  }
}

function summary(t: Trigger) {
  if (t.kind === "npm") return `${t.package_manager} ${t.command}${t.scripts.length ? " " + t.scripts.join(" ") : ""}`.trim();
  return t.script;
}
function hhmm(iso: string) {
  const d = new Date(iso);
  return isNaN(d.getTime()) ? "" : d.toLocaleTimeString("pt-BR", { hour: "2-digit", minute: "2-digit" });
}
const stateLabel = (s: TriggerState) =>
  s.state === "running"
    ? "rodando"
    : s.state === "stopping"
      ? "parando"
      : s.state === "exited"
        ? `saiu (${s.code ?? "sinal"})`
        : s.state === "failed"
          ? "falhou"
          : "parado";
const stateBadge = (s: TriggerState) =>
  s.state === "running"
    ? "running"
    : s.state === "stopping"
      ? "blocked"
      : s.state === "failed" || (s.state === "exited" && s.code !== 0)
        ? "failed"
        : "";
const since = (s: TriggerState) => (s.state === "running" ? `desde ${hhmm(s.since)}` : s.state === "exited" ? hhmm(s.at) : "");

function atBottom() {
  const el = logBox.value;
  return !el || el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}
function scrollEnd() {
  nextTick(() => {
    if (logBox.value) logBox.value.scrollTop = logBox.value.scrollHeight;
  });
}

async function openTrigger(id: string) {
  if (openId.value === id && !editing.value) return;
  openId.value = id;
  editing.value = null;
  confirmDelete.value = false;
  error.value = "";
  log.value = [];
  loadingLog = id;
  pending = [];
  try {
    const lines = await api.triggerLog(id);
    if (openId.value !== id) return;
    log.value = lines.concat(pending).slice(-MAX_LINES);
    scrollEnd();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    if (loadingLog === id) {
      loadingLog = null;
      pending = [];
    }
  }
}

function close() {
  openId.value = null;
  editing.value = null;
  confirmDelete.value = false;
  error.value = "";
}

async function act(fn: (id: string) => Promise<void>, id: string) {
  busy.value[id] = true;
  error.value = "";
  try {
    await fn(id);
  } catch (e) {
    error.value = errorText(e);
  } finally {
    delete busy.value[id];
  }
}
/** O backend recria o arquivo de log a cada execução: a tela começa do zero junto. */
function clearIfOpen(id: string) {
  if (openId.value === id) log.value = [];
}
const start = (id: string) => (clearIfOpen(id), act(api.triggerStart, id));
const stop = (id: string) => act(api.triggerStop, id);
const restart = (id: string) => (clearIfOpen(id), act(api.triggerRestart, id));

const isRunning = (s: TriggerState) => s.state === "running";
const canStart = (s: TriggerState) => s.state !== "running" && s.state !== "stopping";

async function copyLog() {
  try {
    await writeText(log.value.join("\n"));
    copied.value = true;
    setTimeout(() => (copied.value = false), 2000);
  } catch (e) {
    error.value = errorText(e);
  }
}

function duplicate(t: Trigger) {
  editing.value = { ...t, id: "", name: `${t.name} (cópia)`, scripts: [...t.scripts], env: t.env.map(([k, v]) => [k, v]) };
}

async function remove(id: string) {
  confirmDelete.value = false;
  await act(api.triggerDelete, id);
  if (!error.value) close();
  await load();
}

async function saved(t: Trigger) {
  editing.value = null;
  await load();
  openId.value = null;
  await openTrigger(t.id);
}

onMounted(async () => {
  await load();
  unlisten.push(
    await events.onTriggerStatus((e) => {
      const v = views.value.find((x) => x.trigger.id === e.id);
      if (v) v.state = e.state;
      else load();
    }),
    await events.onTriggerLog((e) => {
      if (e.id !== openId.value) return;
      if (loadingLog === e.id) {
        pending = pending.concat(e.lines);
        return;
      }
      const follow = atBottom();
      log.value = log.value.concat(e.lines).slice(-MAX_LINES);
      if (follow) scrollEnd();
    }),
  );
});
onUnmounted(() => unlisten.forEach((u) => u()));
</script>

<template>
  <div :class="['prs-layout', { 'with-panel': showPanel }]">
    <div class="prs">
      <div class="toolbar">
        <button @click="editing = newTrigger()">+ Novo gatilho</button>
        <span class="muted small">comandos que você roda sempre, com o log ao lado</span>
      </div>
      <p v-if="error && !showPanel" class="msg error">{{ error }}</p>

      <section class="card">
        <h2 class="section-head">
          <span>Gatilhos <span class="muted">({{ views.length }})</span></span>
        </h2>
        <p v-if="!views.length" class="muted small">
          Nenhum gatilho cadastrado. Exemplo: <code>npm run serve</code> em <code>~/Projects/acme-front</code>.
        </p>
        <table v-else>
          <tbody>
            <tr
              v-for="v in views"
              :key="v.trigger.id"
              :class="['clickable', { selected: openId === v.trigger.id }]"
              @click="openTrigger(v.trigger.id)"
            >
              <td class="pr-main">
                <span class="title">{{ v.trigger.name }}</span>
                <div class="muted small">
                  {{ v.trigger.kind === "npm" ? "npm" : "shell" }} · <code>{{ summary(v.trigger) }}</code>
                  <template v-if="since(v.state)"> · {{ since(v.state) }}</template>
                </div>
                <span v-if="v.error" class="muted small">{{ v.error }}</span>
              </td>
              <td class="pr-action">
                <span :class="['badge', stateBadge(v.state)]">{{ stateLabel(v.state) }}</span>
                <div class="actions">
                  <button
                    v-if="isRunning(v.state)"
                    class="secondary"
                    :disabled="busy[v.trigger.id]"
                    @click.stop="stop(v.trigger.id)"
                  >Parar</button>
                  <button v-else :disabled="busy[v.trigger.id] || !canStart(v.state) || !!v.error" @click.stop="start(v.trigger.id)">Rodar</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </section>
    </div>

    <aside v-if="editing" class="panel">
      <header class="panel-head">
        <div>
          <h2>{{ editing.id ? "Editar gatilho" : "Novo gatilho" }}</h2>
        </div>
        <button class="secondary" @click="editing = null">Fechar</button>
      </header>
      <div class="panel-scroll">
        <TriggerForm :key="editing.id + editing.name" :trigger="editing" @saved="saved" @cancel="editing = null" />
      </div>
    </aside>

    <aside v-else-if="open" class="panel">
      <header class="panel-head">
        <div>
          <h2>{{ open.trigger.name }}</h2>
          <div class="muted small">
            {{ open.trigger.kind }} · {{ open.trigger.kind === "npm" ? open.trigger.package_json : open.trigger.workdir }}
          </div>
        </div>
        <div class="actions">
          <button v-if="isRunning(open.state)" class="secondary" :disabled="busy[open.trigger.id]" @click="stop(open.trigger.id)">Parar</button>
          <button v-else :disabled="busy[open.trigger.id] || !canStart(open.state) || !!open.error" @click="start(open.trigger.id)">Rodar</button>
          <button
            class="secondary"
            :disabled="busy[open.trigger.id] || open.state.state === 'stopping' || !!open.error"
            @click="restart(open.trigger.id)"
          >Reiniciar</button>
          <button class="secondary" @click="close">Fechar</button>
        </div>
      </header>
      <div class="panel-scroll trigger-scroll">
        <p v-if="error" class="msg error">{{ error }}</p>
        <p v-if="open.error" class="msg error">{{ open.error }}</p>
        <p v-if="open.state.state === 'failed'" class="msg error">{{ open.state.message }}</p>
        <div class="muted small">
          <span :class="['badge', stateBadge(open.state)]">{{ stateLabel(open.state) }}</span>
          <code> $ {{ open.preview }}</code>
        </div>
        <pre ref="logBox" class="log log-fill">{{ log.join("\n") || "sem saída ainda" }}</pre>
        <div class="actions">
          <button class="secondary" @click="log = []">Limpar</button>
          <button class="secondary" @click="copyLog">{{ copied ? "Copiado" : "Copiar" }}</button>
          <button class="secondary" @click="editing = open.trigger">Editar</button>
          <button class="secondary" @click="duplicate(open.trigger)">Duplicar</button>
          <button class="secondary" :disabled="isRunning(open.state) || open.state.state === 'stopping'" @click="confirmDelete = true">Apagar</button>
        </div>
        <div v-if="confirmDelete" class="msg warn">
          Apagar o gatilho {{ open.trigger.name }}? O log também some.
          <div class="alert-actions">
            <button class="small-btn" @click="remove(open.trigger.id)">Apagar mesmo assim</button>
            <button class="small-btn" @click="confirmDelete = false">Cancelar</button>
          </div>
        </div>
      </div>
    </aside>
  </div>
</template>
