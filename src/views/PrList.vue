<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, cardUrl, errorText, events, formatDate, type Pr, type SyncResult } from "../api";
import ReviewPanel from "./ReviewPanel.vue";

const emit = defineEmits<{ (e: "go-mine"): void }>();

const prs = ref<Pr[]>([]);
const lastSync = ref<string | null>(null);
const syncing = ref(false);
const syncInfo = ref<SyncResult | null>(null);
const error = ref("");
const open = ref<{ pr: Pr; runId: string } | null>(null);
const notice = ref("");
const checkingOut = ref<number | null>(null);
const unlisten: (() => void)[] = [];

async function load() {
  try {
    const r = await api.listPrs();
    prs.value = r.prs;
    lastSync.value = r.last_sync;
    if (open.value) {
      const fresh = r.prs.find((p) => p.id === open.value!.pr.id);
      if (fresh) open.value = { ...open.value, pr: fresh };
    }
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

/** PR cujo último review já cobre os mesmos commits: espera confirmação antes de gastar de novo. */
const confirmRepeat = ref<Pr | null>(null);

async function review(pr: Pr, force = false) {
  error.value = "";
  if (!force && pr.same_commits_reviewed_at) {
    confirmRepeat.value = pr;
    return;
  }
  confirmRepeat.value = null;
  try {
    const runId = await api.reviewStart(pr.id);
    open.value = { pr, runId };
    await load();
  } catch (e) {
    error.value = errorText(e);
  }
}

async function checkout(pr: Pr) {
  error.value = "";
  notice.value = "";
  checkingOut.value = pr.id;
  try {
    const r = await api.checkoutPr(pr.id);
    notice.value = r.at_pr_commit
      ? `${r.repo} agora está em ${r.branch} (${r.head.slice(0, 10)}), no commit do PR. O teste no navegador já pode rodar.`
      : `${r.repo} está em ${r.branch} (${r.head.slice(0, 10)}), mas não no commit que o app conhece: clique em Atualizar.`;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    checkingOut.value = null;
  }
}

function show(pr: Pr) {
  if (pr.last_run) open.value = { pr, runId: pr.last_run.id };
}

const showApproved = ref(false);
const toReviewAll = computed(() => prs.value.filter((p) => p.category === "review_requested"));
const approvedHidden = computed(() => toReviewAll.value.filter((p) => p.approved_by_me).length);
const filter = ref("");
const term = computed(() => filter.value.trim().toLowerCase());
const matches = (p: Pr) =>
  [`${p.repo}#${p.bb_id}`, p.title, p.source_branch, p.author ?? ""].some((s) => s.toLowerCase().includes(term.value));
// ---- painel de abertura: os números do dia; cada bloco filtra a lista ----
type Focus = "all" | "never" | "outdated";
const focus = ref<Focus>("all");
const pending = computed(() => toReviewAll.value.filter((p) => !p.approved_by_me));
const focusTest: Record<Focus, (p: Pr) => boolean> = {
  all: () => true,
  // pelo último review concluído: PR cujo único review falhou ou foi cancelado conta como nunca revisado
  never: (p) => !p.reviewed_sha,
  outdated: (p) => p.needs_rereview,
};
const mineChanges = computed(() => prs.value.filter((p) => p.category !== "review_requested" && p.changes_requested).length);
const mineOpen = computed(() => prs.value.filter((p) => p.category !== "review_requested").length);
const tiles = computed(() => [
  { key: "all" as const, label: "Para revisar", n: pending.value.length, sub: `${approvedHidden.value} que você já aprovou ficam fora`, tone: "hot" },
  { key: "never" as const, label: "Nunca revisados", n: pending.value.filter(focusTest.never).length, sub: "sem nenhum review do app", tone: "" },
  { key: "outdated" as const, label: "Review desatualizado", n: pending.value.filter(focusTest.outdated).length, sub: "commits depois do último review", tone: "alert" },
]);
const withHigh = computed(
  () => pending.value.filter((p) => p.last_run?.status === "completed" && p.last_run.high > 0).length,
);
const improvementsTotal = computed(() =>
  pending.value.reduce((n, p) => n + (p.last_run?.status === "completed" ? p.last_run.improvements : 0), 0),
);
function toggleFocus(k: Focus) {
  focus.value = focus.value === k ? "all" : k;
}

const toReview = computed(() =>
  toReviewAll.value.filter(
    (p) => (showApproved.value || !p.approved_by_me) && (!term.value || matches(p)) && focusTest[focus.value](p),
  ),
);
// os meus PRs ficam na aba própria (correção dos comentários): não faz sentido revisar o próprio PR
const sections = computed(() => [{ title: "Para eu revisar", list: toReview.value, review: true }]);

const approvals = (pr: Pr) => pr.reviewers.filter((r) => r.approved).length;
const repoErrors = computed(() => (syncInfo.value?.repos ?? []).filter((r) => r.error));
const fetchWarnings = computed(() => (syncInfo.value?.repos ?? []).filter((r) => r.fetch_error));

// a lista relê o banco sozinha: o timer das 10h atualiza com o app aberto
let timer: ReturnType<typeof setInterval> | undefined;
const onFocus = () => load();

onMounted(async () => {
  await load();
  unlisten.push(await events.onReviewUpdate(() => load()));
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
        <input v-model="filter" class="filter" type="search" placeholder="Filtrar PR, título, branch ou autor" />
        <span class="muted small">
          última atualização: {{ formatDate(lastSync) }}
          <template v-if="syncInfo"> · {{ syncInfo.requests }} chamadas · como {{ syncInfo.user }}</template>
        </span>
      </div>
      <p v-if="error" class="msg error">{{ error }}</p>
      <p v-if="notice" class="msg ok">{{ notice }}</p>
      <div v-if="confirmRepeat" class="msg warn">
        {{ confirmRepeat.repo }}#{{ confirmRepeat.bb_id }} já tem um review concluído em
        {{ formatDate(confirmRepeat.same_commits_reviewed_at) }} nos mesmos commits de origem e destino.
        Revisar de novo repete o mesmo trabalho e o custo. Se o PR recebeu commits depois da última
        atualização, clique em Atualizar antes.
        <div class="alert-actions">
          <button class="small-btn" @click="review(confirmRepeat, true)">Revisar mesmo assim</button>
          <button class="small-btn" @click="confirmRepeat = null">Cancelar</button>
        </div>
      </div>
      <p v-for="r in repoErrors" :key="r.slug" class="msg error">{{ r.slug }}: {{ r.error }}</p>
      <p v-for="r in fetchWarnings" :key="'f' + r.slug" class="msg warn">
        git fetch de {{ r.slug }} falhou (o Checkout usa o que já está no clone): {{ r.fetch_error }}
      </p>

      <div class="mission" role="group" aria-label="Números do dia">
        <button
          v-for="t in tiles"
          :key="t.key"
          type="button"
          :class="['tile', t.tone, { zero: !t.n, on: focus === t.key && t.key !== 'all' }]"
          :aria-pressed="focus === t.key"
          @click="toggleFocus(t.key)"
        >
          <span class="tile-label">{{ t.label }}</span>
          <span class="tile-n">{{ t.n }}</span>
          <span class="tile-sub">{{ t.sub }}</span>
        </button>
        <button type="button" :class="['tile', { zero: !mineChanges, alert: mineChanges > 0 }]" @click="emit('go-mine')">
          <span class="tile-label">Meus PRs com mudança pedida</span>
          <span class="tile-n">{{ mineChanges }}</span>
          <span class="tile-sub">{{ mineOpen }} PR(s) seu(s) aberto(s) · abrir Meus PRs</span>
        </button>
      </div>
      <p class="mission-strip">
        <span><b>{{ withHigh }}</b> review(s) com achado HIGH ou mais</span>
        <span><b>{{ improvementsTotal }}</b> melhoria(s) nos últimos reviews</span>
        <span v-if="focus !== 'all'">
          mostrando só: {{ focus === "never" ? "nunca revisados" : "review desatualizado" }}
          · <button class="link" @click="focus = 'all'">mostrar todos</button>
        </span>
      </p>

      <section v-for="s in sections" :key="s.title" class="card">
        <h2 class="section-head">
          <span>{{ s.title }} <span class="muted">({{ s.list.length }})</span></span>
          <label v-if="s.review" class="check">
            <input v-model="showApproved" type="checkbox" />
            Mostrar os que já aprovei
            <span class="muted">({{ approvedHidden }})</span>
          </label>
        </h2>
        <p v-if="!s.list.length" class="muted small">{{ term ? "Nenhum PR encontrado." : "Nenhum PR aberto." }}</p>
        <table v-else>
          <tbody>
            <tr
              v-for="pr in s.list"
              :key="pr.id"
              :class="{ selected: open?.pr.id === pr.id, clickable: !!pr.last_run }"
              @click="show(pr)"
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
                  → <code>{{ pr.dest_branch }}</code> · {{ pr.author }} ·
                  {{ formatDate(pr.updated_on) }}
                  <span
                    v-if="pr.needs_rereview"
                    class="badge warn"
                  >atualizado após o review</span>
                </div>
                <div class="reviewers">
                  <span v-if="pr.approved_by_me" class="badge ok">aprovei</span>
                  <span v-if="pr.changes_requested" class="badge error">pediram mudanças</span>
                  <span class="muted small">{{ approvals(pr) }}/{{ pr.reviewers.length }} aprovações</span>
                  <span
                    v-for="r in pr.reviewers"
                    :key="r.uuid ?? r.name"
                    :class="['chip', r.state === 'changes_requested' ? 'cr' : r.approved ? 'ap' : '']"
                  >{{ r.state === "changes_requested" ? "✗" : r.approved ? "✓" : "·" }} {{ r.name }}</span>
                </div>
              </td>
              <td class="pr-action">
                <template v-if="pr.last_run">
                  <a href="#" class="small" @click.prevent.stop="show(pr)">
                    <span :class="['badge', pr.last_run.status]">{{ pr.last_run.status === "running" ? "revisando" : pr.last_run.status }}</span>
                    <span v-if="pr.last_run.warning" class="badge warn">sem branch-reviewer</span>
                    <template v-if="pr.last_run.status === 'completed'">
                      {{ pr.last_run.findings }} achado(s)<template v-if="pr.last_run.high">, {{ pr.last_run.high }} high+</template><template v-if="pr.last_run.improvements"> · {{ pr.last_run.improvements }} melhoria(s)</template>
                    </template>
                  </a>
                </template>
                <div class="actions">
                  <button
                    class="secondary"
                    :disabled="checkingOut !== null"
                    @click.stop="checkout(pr)"
                  >
                    {{ checkingOut === pr.id ? "Trocando…" : "Checkout" }}
                  </button>
                  <button
                    :class="{ secondary: !!pr.last_run && !pr.needs_rereview && pr.last_run.status === 'completed' }"
                    :disabled="pr.last_run?.status === 'running'"
                    @click.stop="review(pr)"
                  >
                    {{ pr.last_run ? "Revisar de novo" : "Revisar" }}
                  </button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </section>
    </div>

    <ReviewPanel
      v-if="open"
      :key="open.runId"
      :pr="open.pr"
      :run-id="open.runId"
      @close="open = null"
      @rerun="(id) => { open = open && { ...open, runId: id }; load(); }"
    />
  </div>
</template>
