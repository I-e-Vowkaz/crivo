<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api, errorText, formatDate, jiraSite, normalizeJiraSite, type AppInfo, type BitbucketAccount, type TestApp, type TestAppView } from "../api";

const info = ref<AppInfo | null>(null);
const email = ref("");
const workspace = ref("");
const repos = ref("");
const reposRoot = ref("");
const jiraSiteInput = ref("");
const tokenSaved = ref(false);
const tokenInput = ref("");
const account = ref<BitbucketAccount | null>(null);
const testApps = ref<(TestAppView & { reposText: string; hostsText: string; newPassword: string })[]>([]);
const busy = ref(false);
/** Projetos ocultados na tela "Minhas branches". */
const hiddenProjects = ref<string[]>([]);
const message = ref<{ kind: "ok" | "error"; text: string } | null>(null);

async function load() {
  try {
    info.value = await api.appInfo();
    email.value = (await api.settingGet<string>("bitbucket.email")) ?? "";
    workspace.value = (await api.settingGet<string>("bitbucket.workspace")) ?? "";
    const r = await api.settingGet<string[] | string>("bitbucket.repos");
    repos.value = Array.isArray(r) ? r.join(", ") : (r ?? "");
    reposRoot.value = (await api.settingGet<string>("repos.root")) ?? "";
    jiraSiteInput.value = (await api.settingGet<string>("jira.site")) ?? "";
    tokenSaved.value = (await api.secretStatus("bitbucket")).saved;
    account.value = await api.bitbucketAccount();
    setTestApps(await api.testAppsGet());
    hiddenProjects.value = (await api.settingGet<string[]>("branches.hidden")) ?? [];
  } catch (e) {
    message.value = { kind: "error", text: errorText(e) };
  }
}

async function run(action: () => Promise<void>, ok: string) {
  busy.value = true;
  message.value = null;
  try {
    await action();
    message.value = { kind: "ok", text: ok };
  } catch (e) {
    message.value = { kind: "error", text: errorText(e) };
  } finally {
    busy.value = false;
  }
}

const hiddenMsg = ref<{ kind: "ok" | "error"; text: string } | null>(null);

async function showProject(slug: string) {
  hiddenMsg.value = null;
  try {
    const rest = hiddenProjects.value.filter((x) => x !== slug);
    await api.settingSet("branches.hidden", rest);
    hiddenProjects.value = rest;
    hiddenMsg.value = { kind: "ok", text: `${slug} volta a aparecer em Minhas branches.` };
  } catch (e) {
    hiddenMsg.value = { kind: "error", text: errorText(e) };
  }
}

function setTestApps(v: TestAppView[]) {
  testApps.value = v.map((a) => ({ ...a, reposText: a.repos.join(", "), hostsText: a.allowed_hosts.join(", "), newPassword: "" }));
}

const splitList = (s: string) => s.split(",").map((x) => x.trim()).filter(Boolean);
const plainApps = (): TestApp[] =>
  testApps.value.map((a) => ({
    id: a.id,
    name: a.name,
    base_url: a.base_url.trim(),
    login_url: a.login_url.trim(),
    username: a.username.trim(),
    allowed_hosts: splitList(a.hostsText),
    repos: splitList(a.reposText),
  }));

const saveTestApp = (id: string) =>
  run(async () => {
    const app = testApps.value.find((a) => a.id === id);
    setTestApps(await api.testAppSave(plainApps(), id, app?.newPassword || undefined));
  }, "Ambiente de teste salvo.");

const newAppName = ref("");

function addTestApp() {
  const name = newAppName.value.trim();
  if (!name) return;
  newAppName.value = "";
  const id = name.toLowerCase().normalize("NFD").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  if (!id || testApps.value.some((a) => a.id === id)) return;
  testApps.value.push({
    id, name, base_url: "", login_url: "", username: "", allowed_hosts: [], repos: [],
    password_saved: false, effective_hosts: [], reposText: "", hostsText: "", newPassword: "",
  });
}

const saveAccount = () =>
  run(async () => {
    await api.settingSet("bitbucket.email", email.value.trim());
    await api.settingSet("bitbucket.workspace", workspace.value.trim());
    await api.settingSet(
      "bitbucket.repos",
      repos.value.split(",").map((s) => s.trim()).filter(Boolean),
    );
    await api.settingSet("repos.root", reposRoot.value.trim());
    const site = normalizeJiraSite(jiraSiteInput.value);
    await api.settingSet("jira.site", site);
    jiraSiteInput.value = site;
    jiraSite.value = site;
  }, "Configurações salvas.");

const saveToken = () =>
  run(async () => {
    account.value = await api.bitbucketSaveToken(tokenInput.value);
    tokenSaved.value = true;
    tokenInput.value = "";
  }, "Token salvo.");

const deleteToken = () =>
  run(async () => {
    tokenSaved.value = (await api.secretDelete("bitbucket")).saved;
    account.value = null;
  }, "Token removido.");

onMounted(load);
</script>

<template>
  <section class="card">
    <h2>Bitbucket</h2>
    <form class="grid" @submit.prevent="saveAccount">
      <label>
        Email da conta Atlassian
        <input v-model="email" type="email" autocomplete="off" placeholder="voce@empresa.com.br" />
      </label>
      <label>
        Workspace
        <input v-model="workspace" type="text" autocomplete="off" placeholder="minha-empresa" />
      </label>
      <label class="wide">
        Repositórios monitorados (separados por vírgula)
        <input v-model="repos" type="text" autocomplete="off" placeholder="meu-back, meu-front" />
      </label>
      <label class="wide">
        Pasta dos clones locais (cada repo em &lt;pasta&gt;/&lt;repo&gt;)
        <input v-model="reposRoot" type="text" autocomplete="off" placeholder="~/Projects (padrão)" />
      </label>
      <label class="wide">
        Site do Jira (link do card e leitura do card pelo MCP do Atlassian)
        <input v-model="jiraSiteInput" type="text" autocomplete="off" placeholder="empresa.atlassian.net" />
      </label>
      <div><button type="submit" :disabled="busy">Salvar</button></div>
    </form>

    <div class="token">
      <div>
        API token
        <span :class="['badge', tokenSaved ? 'ok' : 'off']">{{ tokenSaved ? "salvo" : "não salvo" }}</span>
        <span v-if="account" class="muted small">— {{ account.user.display_name }}, validado em {{ formatDate(account.validated_at) }}</span>
      </div>
      <form @submit.prevent="saveToken">
        <input
          v-model="tokenInput"
          type="password"
          autocomplete="off"
          :placeholder="tokenSaved ? 'colar um token novo para substituir' : 'colar o token'"
        />
        <button type="submit" :disabled="busy || !tokenInput.trim()">Salvar token</button>
        <button v-if="tokenSaved" type="button" class="secondary" :disabled="busy" @click="deleteToken">Remover</button>
      </form>
      <p class="muted small">
        Somente leitura: <code>read:pullrequest</code>, <code>read:repository</code>, <code>read:user</code>. Fica no
        cofre do sistema e nunca volta para esta tela.
      </p>
    </div>

    <p v-if="message" :class="['msg', message.kind]">{{ message.text }}</p>
  </section>

  <section class="card">
    <h2>Ambientes de teste (navegador)</h2>
    <p class="muted small">
      O teste no navegador só roda quando o seu clone do repositório está no commit do PR (use o botão Checkout na
      lista). Use apenas usuário e senha de <strong>desenvolvimento</strong>.
    </p>
    <p v-if="!testApps.length" class="muted small">Nenhum ambiente configurado.</p>
    <form v-for="a in testApps" :key="a.id" class="grid test-app" @submit.prevent="saveTestApp(a.id)">
      <h3 class="wide-row">{{ a.name }} <code class="muted small">{{ a.id }}</code></h3>
      <label>URL da aplicação local<input v-model="a.base_url" type="url" placeholder="http://localhost:8080" /></label>
      <label>URL de login (opcional)<input v-model="a.login_url" type="url" /></label>
      <label>Usuário de teste<input v-model="a.username" type="text" autocomplete="off" /></label>
      <label>
        Senha de teste <span :class="['badge', a.password_saved ? 'ok' : 'off']">{{ a.password_saved ? "salva" : "não salva" }}</span>
        <input v-model="a.newPassword" type="password" autocomplete="off" :placeholder="a.password_saved ? 'manter a atual' : ''" />
      </label>
      <label class="wide">Repositórios testados aqui (vírgula)<input v-model="a.reposText" type="text" /></label>
      <label class="wide">
        Hosts permitidos no Chrome (vazio = {{ a.effective_hosts.join(", ") || "host da URL + localhost" }})
        <input v-model="a.hostsText" type="text" />
      </label>
      <div><button type="submit" :disabled="busy">Salvar ambiente</button></div>
    </form>
    <form @submit.prevent="addTestApp">
      <input v-model="newAppName" type="text" placeholder="nome do novo ambiente (ex.: Beta)" />
      <button class="secondary" type="submit" :disabled="!newAppName.trim()">Adicionar ambiente</button>
    </form>
  </section>

  <section class="card">
    <h2>Minhas branches</h2>
    <p class="muted small">Projetos ocultados na tela Minhas branches. Mostrar devolve o projeto para a lista.</p>
    <p v-if="!hiddenProjects.length" class="muted small">Nenhum projeto oculto.</p>
    <ul v-else class="hidden-list">
      <li v-for="slug in hiddenProjects" :key="slug">
        <code>{{ slug }}</code>
        <button type="button" class="secondary small-btn" @click="showProject(slug)">Mostrar</button>
      </li>
    </ul>
    <p v-if="hiddenMsg" :class="['msg', hiddenMsg.kind]">{{ hiddenMsg.text }}</p>
  </section>

  <section v-if="info" class="card">
    <h2>Sobre</h2>
    <dl>
      <dt>Versão</dt>
      <dd>{{ info.version }}</dd>
      <dt>Banco</dt>
      <dd><code>{{ info.db_path }}</code></dd>
      <dt>Schema</dt>
      <dd>{{ info.schema_version }} de {{ info.schema_latest }}</dd>
      <dt>Cofre do sistema</dt>
      <dd>
        <span v-if="!info.keyring_error" class="badge ok">acessível</span>
        <span v-else class="badge error">{{ info.keyring_error }}</span>
      </dd>
    </dl>
  </section>
</template>
