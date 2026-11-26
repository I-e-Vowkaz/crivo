<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, cardUrl, errorText, events, formatDate, type FixSummary, type Pr, type SyncResult } from "../api";
import FixPanel from "./FixPanel.vue";

const prs = ref<Pr[]>([]);
const lastSync = ref<string | null>(null);
const syncing = ref(false);
const syncInfo = ref<SyncResult | null>(null);
const error = ref("");
const openId = ref<number | null>(null);
const unlisten: (() => void)[] = [];

async function load() {
  try {
    const r = await api.listPrs();
    prs.value = r.prs;
    lastSync.value = r.last_sync;
  } catch (e) {
    error.value = errorText(e);
  }
}

async function sync() {
  syncing.value = true;
  error.value = "";
  try {
    syncInfo.value = await api.syncPrs();
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    syncing.value = false;
  }
}

/** Só os PRs que eu abri: o review fica na aba "Para eu revisar". */
const mine = computed(() => prs.value.filter((p) => p.category !== "review_requested"));
const open = computed(() => mine.value.find((p) => p.id === openId.value) ?? null);
const repoErrors = computed(() => (syncInfo.value?.repos ?? []).filter((r) => r.error));

const fixLabel: Record<FixSummary["status"], string> = {
  analyzing: "analisando",
  ready: "pronto",
  failed: "falhou",
  cancelled: "cancelado",
  committing: "commit e push",
  committed: "commit feito",
  pushed: "enviado",
};
const fixBadge = (f: FixSummary) =>
  f.status === "pushed" ? "completed" : f.status === "analyzing" || f.status === "committing" ? "running" : f.status;

let timer: ReturnType<typeof setInterval> | undefined;
const onFocus = () => load();

onMounted(async () => {
  await load();
  unlisten.push(await events.onFixUpdate((e) => e.line === null && load()));
  timer = setInterval(load, 30_000);
  window.addEventListener("focus", onFocus);
  if (!prs.value.length) sync();
});
onUnmounted(() => {
  unlisten.forEach((u) => u());
  if (timer) clearInterval(timer);
  window.removeEventListener("focus", onFocus);
});
</script>

<template>
  <div :class="['prs-layout', { 'with-panel': open }]">
    <div class="prs">
      <div class="toolbar">
        <button :disabled="syncing" @click="sync">{{ syncing ? "Atualizando…" : "Atualizar" }}</button>
        <span class="muted small">
          última atualização: {{ formatDate(lastSync) }}
          <template v-if="syncInfo"> · {{ syncInfo.requests }} chamadas · como {{ syncInfo.user }}</template>
        </span>
      </div>
      <p v-if="error" class="msg error">{{ error }}</p>
      <p v-for="r in repoErrors" :key="r.slug" class="msg error">{{ r.slug }}: {{ r.error }}</p>

      <section class="card">
        <h2 class="section-head">
          <span>Meus PRs <span class="muted">({{ mine.length }})</span></span>
        </h2>
        <p v-if="!mine.length" class="muted small">Nenhum PR aberto.</p>
        <table v-else>
          <tbody>
            <tr
              v-for="pr in mine"
              :key="pr.id"
              :class="['clickable', { selected: openId === pr.id }]"
              @click="openId = pr.id"
            >
              <td class="pr-main">
                <a href="#" @click.prevent.stop="openUrl(pr.url)">{{ pr.repo }}#{{ pr.bb_id }}</a>
                <span class="title">{{ pr.title }}</span>
                <div class="muted small">
                  <a
                    v-if="cardUrl(pr.source_branch)"
                    href="#"
                    title="Abrir o card no Jira"
                    @click.prevent.stop="openUrl(cardUrl(pr.source_branch)!)"
                    ><code>{{ pr.source_branch }}</code></a
                  ><code v-else>{{ pr.source_branch }}</code>
                  → <code>{{ pr.dest_branch }}</code> · {{ formatDate(pr.updated_on) }}
                </div>
                <div class="reviewers">
                  <span v-if="pr.changes_requested" class="badge error">pediram mudanças</span>
                  <span class="muted small">{{ pr.comment_count ?? "?" }} comentário(s)</span>
                  <span
                    v-for="r in pr.reviewers"
                    :key="r.uuid ?? r.name"
                    :class="['chip', r.state === 'changes_requested' ? 'cr' : r.approved ? 'ap' : '']"
                  >{{ r.state === "changes_requested" ? "✗" : r.approved ? "✓" : "·" }} {{ r.name }}</span>
                </div>
              </td>
              <td class="pr-action">
                <span v-if="pr.fix" class="small">
                  <span :class="['badge', fixBadge(pr.fix)]">{{ fixLabel[pr.fix.status] }}</span>
                  <template v-if="pr.fix.total">
                    {{ pr.fix.applied }}/{{ pr.fix.total }} aplicado(s)<template v-if="pr.fix.pending"> · {{ pr.fix.pending }} esperando aprovação</template>
                  </template>
                </span>
                <div class="actions">
                  <button :class="{ secondary: !!pr.fix }" @click.stop="openId = pr.id">Comentários</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </section>
    </div>

    <FixPanel v-if="open" :key="open.id" :pr="open" @close="openId = null" />
  </div>
</template>
