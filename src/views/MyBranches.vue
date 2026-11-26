<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, cardUrl, errorText, events, formatDate, type FixSummary, type LocalBranch, type LocalProject } from "../api";
import BranchPanel from "./BranchPanel.vue";

const PAGE = 10;

const projects = ref<LocalProject[]>([]);
const loading = ref(false);
const error = ref("");
const filter = ref("");
/** Projeto aberto (dropdown) e página de cada um. */
const opened = reactive<Record<string, boolean>>({});
const page = reactive<Record<string, number>>({});
const open = ref<{ slug: string; name: string } | null>(null);
const confirmRepeat = ref<{ slug: string; branch: LocalBranch } | null>(null);
const starting = ref("");
const notice = ref("");

/** Tira o projeto da lista; ele volta pelas Configurações ("Minhas branches"). */
async function hide(slug: string) {
  error.value = "";
  try {
    const hidden = (await api.settingGet<string[]>("branches.hidden")) ?? [];
    await api.settingSet("branches.hidden", [...new Set([...hidden, slug])].sort());
    if (open.value?.slug === slug) open.value = null;
    notice.value = `${slug} saiu da lista. Para exibir de novo, vá em Configurações.`;
    await load();
  } catch (e) {
    error.value = errorText(e);
  }
}
const unlisten: (() => void)[] = [];

async function load() {
  loading.value = true;
  try {
    projects.value = await api.localBranches();
    error.value = "";
    // abre o projeto mais recente na primeira carga
    const first = projects.value.find((p) => p.branches.length);
    if (first && !Object.keys(opened).length) opened[first.slug] = true;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    loading.value = false;
  }
}

const term = computed(() => filter.value.trim().toLowerCase());
const branchesOf = (p: LocalProject) =>
  term.value ? p.branches.filter((b) => b.name.toLowerCase().includes(term.value) || b.subject.toLowerCase().includes(term.value)) : p.branches;
const visible = computed(() => projects.value.filter((p) => !term.value || branchesOf(p).length || p.slug.toLowerCase().includes(term.value)));
const pages = (p: LocalProject) => Math.max(1, Math.ceil(branchesOf(p).length / PAGE));
const pageOf = (p: LocalProject) => Math.min(page[p.slug] ?? 0, pages(p) - 1);
const shown = (p: LocalProject) => branchesOf(p).slice(pageOf(p) * PAGE, pageOf(p) * PAGE + PAGE);

const openBranch = computed(() => {
  if (!open.value) return null;
  const b = projects.value.find((p) => p.slug === open.value!.slug)?.branches.find((x) => x.name === open.value!.name);
  return b ? { slug: open.value.slug, branch: b } : null;
});

function toggle(slug: string, e: Event) {
  opened[slug] = (e.target as HTMLDetailsElement).open;
}

async function review(slug: string, b: LocalBranch, force = false) {
  error.value = "";
  // o último review já cobre o commit atual da branch: confirma antes de gastar de novo
  if (!force && b.last_run?.status === "completed" && !b.last_run.outdated) {
    confirmRepeat.value = { slug, branch: b };
    return;
  }
  confirmRepeat.value = null;
  starting.value = `${slug}/${b.name}`;
  try {
    await api.reviewStartBranch(slug, b.name);
    open.value = { slug, name: b.name };
    await load();
  } catch (e) {
    error.value = errorText(e);
  } finally {
    starting.value = "";
  }
}

const fixLabel: Record<FixSummary["status"], string> = {
  analyzing: "corrigindo",
  ready: "correção pronta",
  failed: "correção falhou",
  cancelled: "correção descartada",
  committing: "commit",
  committed: "commit feito",
  pushed: "enviado",
};
const fixBadge = (f: FixSummary) =>
  f.status === "pushed" || f.status === "committed" ? "completed" : f.status === "analyzing" || f.status === "committing" ? "running" : f.status;
const kindLabel: Record<LocalProject["kind"], string> = { back: "back", front: "front", outro: "outro" };

let timer: ReturnType<typeof setInterval> | undefined;
const onFocus = () => load();

onMounted(async () => {
  await load();
  unlisten.push(await events.onReviewUpdate((e) => e.pr_id === 0 && load()));
  unlisten.push(await events.onFixUpdate((e) => e.pr_id === 0 && e.line === null && load()));
  timer = setInterval(load, 30_000);
  window.addEventListener("focus", onFocus);
});
onUnmounted(() => {
  unlisten.forEach((u) => u());
  if (timer) clearInterval(timer);
  window.removeEventListener("focus", onFocus);
});
</script>

<template>
  <div :class="['prs-layout', { 'with-panel': openBranch }]">
    <div class="prs">
      <div class="toolbar">
        <button :disabled="loading" @click="load">{{ loading ? "Lendo…" : "Atualizar" }}</button>
        <input v-model="filter" class="filter" type="search" placeholder="Filtrar branch ou commit" />
        <span class="muted small">branches locais dos clones da pasta configurada, sem rede</span>
      </div>
      <p v-if="error" class="msg error">{{ error }}</p>
      <p v-if="notice" class="msg ok">{{ notice }}</p>
      <div v-if="confirmRepeat" class="msg warn">
        {{ confirmRepeat.branch.name }} já tem um review concluído no commit atual. Revisar de novo repete o mesmo trabalho e
        o custo.
        <div class="alert-actions">
          <button class="small-btn" @click="review(confirmRepeat.slug, confirmRepeat.branch, true)">Revisar mesmo assim</button>
          <button class="small-btn" @click="confirmRepeat = null">Cancelar</button>
        </div>
      </div>
      <p v-if="!loading && !visible.length" class="muted">Nenhum clone git na pasta configurada.</p>

      <details
        v-for="p in visible"
        :key="p.slug"
        class="card project"
        :open="opened[p.slug] || !!term"
        @toggle="toggle(p.slug, $event)"
      >
        <summary>
          <strong>{{ p.slug }}</strong>
          <span class="badge">{{ kindLabel[p.kind] }}</span>
          <span class="muted small">
            {{ branchesOf(p).length }} branch(es)<template v-if="p.base"> · base origin/{{ p.base }}</template>
            · último commit {{ formatDate(p.last_commit) }}
          </span>
          <button class="secondary small-btn push" title="Volta pelas Configurações" @click.prevent.stop="hide(p.slug)">
            Ocultar
          </button>
        </summary>
        <div class="project-body">
          <p v-if="p.error" class="msg error">{{ p.error }}</p>
          <p v-else-if="!p.base" class="msg warn small">Sem origin/dev nem branch padrão na origin: não dá para revisar.</p>
          <p v-if="!branchesOf(p).length" class="muted small">Nenhuma branch local (fora as protegidas).</p>
          <table v-else>
            <tbody>
              <tr
                v-for="b in shown(p)"
                :key="b.name"
                :class="['clickable', { selected: open?.slug === p.slug && open?.name === b.name }]"
                @click="open = { slug: p.slug, name: b.name }"
              >
                <td class="pr-main">
                  <a
                    v-if="cardUrl(b.name)"
                    href="#"
                    title="Abrir o card no Jira"
                    @click.prevent.stop="openUrl(cardUrl(b.name)!)"
                    ><code>{{ b.name }}</code></a
                  ><code v-else>{{ b.name }}</code>
                  <span v-if="b.head" class="badge ok">aberta</span>
                  <span v-if="b.track" class="muted small"> {{ b.track }}</span>
                  <span class="title">{{ b.subject }}</span>
                  <div class="muted small">
                    {{ formatDate(b.date) }} · <code>{{ b.sha.slice(0, 10) }}</code>
                    <template v-if="b.upstream"> · {{ b.upstream }}</template>
                    <template v-else> · só local</template>
                  </div>
                </td>
                <td class="pr-action">
                  <span v-if="b.last_run" class="small">
                    <span :class="['badge', b.last_run.status]">{{ b.last_run.status === "running" ? "revisando" : b.last_run.status }}</span>
                    <span v-if="b.last_run.outdated" class="badge warn">commits depois do review</span>
                    <template v-if="b.last_run.status === 'completed'">
                      {{ b.last_run.findings }} achado(s)<template v-if="b.last_run.high">, {{ b.last_run.high }} high+</template><template v-if="b.last_run.improvements"> · {{ b.last_run.improvements }} melhoria(s)</template>
                    </template>
                  </span>
                  <span v-if="b.fix" class="small">
                    <span :class="['badge', fixBadge(b.fix)]">{{ fixLabel[b.fix.status] }}</span>
                    <template v-if="b.fix.total">{{ b.fix.applied }}/{{ b.fix.total }} aplicado(s)</template>
                  </span>
                  <div class="actions">
                    <button
                      :class="{ secondary: b.last_run?.status === 'completed' && !b.last_run.outdated }"
                      :disabled="!p.base || b.last_run?.status === 'running' || starting === `${p.slug}/${b.name}`"
                      @click.stop="review(p.slug, b)"
                    >
                      {{ b.last_run ? "Revisar de novo" : "Revisar" }}
                    </button>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-if="pages(p) > 1" class="pager">
            <button class="secondary small-btn" :disabled="pageOf(p) === 0" @click="page[p.slug] = pageOf(p) - 1">Anterior</button>
            <span class="muted small">página {{ pageOf(p) + 1 }} de {{ pages(p) }}</span>
            <button class="secondary small-btn" :disabled="pageOf(p) >= pages(p) - 1" @click="page[p.slug] = pageOf(p) + 1">
              Próxima
            </button>
          </div>
        </div>
      </details>
    </div>

    <BranchPanel
      v-if="openBranch"
      :key="`${openBranch.slug}/${openBranch.branch.name}`"
      :slug="openBranch.slug"
      :branch="openBranch.branch"
      @close="open = null"
      @changed="load"
    />
  </div>
</template>
