<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { api, errorText, type NodeRuntime, type Trigger, type TriggerKind } from "../api";

const props = defineProps<{ trigger: Trigger }>();
const emit = defineEmits<{ saved: [Trigger]; cancel: [] }>();

const t = ref<Trigger>({ ...props.trigger, scripts: [...props.trigger.scripts], env: props.trigger.env.map(([k, v]) => [k, v]) });
const envText = ref(t.value.env.map(([k, v]) => `${k}=${v}`).join("\n"));
const runtimes = ref<NodeRuntime[]>([]);
const available = ref<string[]>([]);
const scriptsError = ref("");
const scriptsText = ref(t.value.scripts.join(", "));
const preview = ref("");
const previewError = ref("");
const error = ref("");
const saving = ref(false);

/** Linhas de variável sem "=" (ou sem nome) bloqueiam o salvar. */
const envLines = computed(() =>
  envText.value
    .split("\n")
    .map((l, i) => ({ n: i + 1, text: l.trim() }))
    .filter((l) => l.text),
);
const badEnv = computed(() => envLines.value.filter((l) => l.text.indexOf("=") < 1));
const parsedEnv = computed<[string, string][]>(() =>
  envLines.value
    .filter((l) => l.text.indexOf("=") >= 1)
    .map((l) => {
      const i = l.text.indexOf("=");
      return [l.text.slice(0, i).trim(), l.text.slice(i + 1)] as [string, string];
    }),
);

function current(): Trigger {
  const out = { ...t.value, env: parsedEnv.value };
  if (out.kind === "npm" && !available.value.length) {
    out.scripts = scriptsText.value.split(",").map((s) => s.trim()).filter(Boolean);
  }
  return out;
}

function setKind(k: TriggerKind) {
  t.value.kind = k;
  if (k === "npm" && !t.value.command) t.value.command = "run";
}

function toggleScript(s: string) {
  const i = t.value.scripts.indexOf(s);
  if (i >= 0) t.value.scripts.splice(i, 1);
  else t.value.scripts.push(s);
}

async function readScripts() {
  scriptsError.value = "";
  available.value = [];
  if (!t.value.package_json.trim()) return;
  try {
    available.value = await api.triggerScripts(t.value.package_json.trim());
  } catch (e) {
    scriptsError.value = errorText(e);
  }
}

let debounce: ReturnType<typeof setTimeout> | undefined;
function schedulePreview() {
  if (debounce) clearTimeout(debounce);
  debounce = setTimeout(async () => {
    try {
      preview.value = await api.triggerPreview(current());
      previewError.value = "";
    } catch (e) {
      previewError.value = errorText(e);
    }
  }, 300);
}
watch([t, envText, scriptsText], schedulePreview, { deep: true });

async function save() {
  if (badEnv.value.length) return;
  saving.value = true;
  error.value = "";
  try {
    emit("saved", await api.triggerSave(current()));
  } catch (e) {
    error.value = errorText(e);
  } finally {
    saving.value = false;
  }
}

onMounted(async () => {
  try {
    runtimes.value = await api.nodeRuntimes();
  } catch {
    /* sem a lista, o campo continua aceitando texto */
  }
  if (t.value.kind === "npm") await readScripts();
  schedulePreview();
});
onUnmounted(() => debounce && clearTimeout(debounce));
</script>

<template>
  <form class="grid trigger-form" @submit.prevent="save">
    <nav class="mode-switch inline wide-row">
      <button type="button" :class="['small-btn', { on: t.kind === 'npm' }]" @click="setKind('npm')">npm</button>
      <button type="button" :class="['small-btn', { on: t.kind === 'shell' }]" @click="setKind('shell')">Shell</button>
    </nav>

    <label class="wide">
      Nome
      <input v-model="t.name" type="text" autocomplete="off" placeholder="front acme" />
    </label>

    <template v-if="t.kind === 'npm'">
      <label class="wide">
        package.json
        <input
          v-model="t.package_json"
          type="text"
          autocomplete="off"
          placeholder="~/Projects/acme-front/package.json"
          @blur="readScripts"
        />
      </label>
      <label>
        Comando
        <input v-model="t.command" type="text" list="trigger-commands" autocomplete="off" placeholder="run" />
        <datalist id="trigger-commands">
          <option value="run" />
          <option value="install" />
          <option value="ci" />
          <option value="test" />
        </datalist>
      </label>
      <div class="field">
        <span class="field-label">Gerenciador</span>
        <nav class="mode-switch inline">
          <button
            v-for="m in ['npm', 'yarn', 'pnpm']"
            :key="m"
            type="button"
            :class="['small-btn', { on: t.package_manager === m }]"
            @click="t.package_manager = m"
          >{{ m }}</button>
        </nav>
      </div>
      <div class="wide-row field">
        <span class="field-label">Scripts</span>
        <template v-if="available.length">
          <label v-for="s in available" :key="s" class="check">
            <input type="checkbox" :checked="t.scripts.includes(s)" @change="toggleScript(s)" />
            {{ s }}
          </label>
        </template>
        <template v-else>
          <input v-model="scriptsText" type="text" autocomplete="off" placeholder="serve, watch (separados por vírgula)" />
          <p v-if="scriptsError" class="msg warn small">{{ scriptsError }}</p>
        </template>
      </div>
      <label class="wide">
        Argumentos
        <input v-model="t.args" type="text" autocomplete="off" placeholder="--port 8081" />
      </label>
    </template>

    <template v-else>
      <label class="wide">
        Pasta
        <input v-model="t.workdir" type="text" autocomplete="off" placeholder="~/Projects/acme-back" />
      </label>
      <label class="wide">
        Comando
        <input v-model="t.script" type="text" autocomplete="off" placeholder="docker compose up" />
      </label>
      <label class="wide">
        Argumentos
        <input v-model="t.args" type="text" autocomplete="off" />
      </label>
      <label class="wide">
        Comando de parada (opcional)
        <input v-model="t.stop_script" type="text" autocomplete="off" placeholder="ex.: docker compose stop" />
      </label>
    </template>

    <label>
      Node
      <input v-model="t.node" type="text" list="trigger-nodes" autocomplete="off" :placeholder="`do sistema${runtimes.find((r) => !r.path) ? ` (${runtimes.find((r) => !r.path)?.version})` : ''}`" />
      <datalist id="trigger-nodes">
        <option v-for="r in runtimes.filter((r) => r.path)" :key="r.path" :value="r.path">{{ r.version }} · {{ r.source }}</option>
      </datalist>
    </label>
    <label v-if="t.kind === 'npm'">
      Opções do Node
      <input v-model="t.node_options" type="text" autocomplete="off" placeholder="--max-old-space-size=4096" />
    </label>

    <label class="wide">
      Variáveis (uma por linha, CHAVE=valor)
      <textarea v-model="envText" rows="4" spellcheck="false" placeholder="NODE_ENV=development"></textarea>
    </label>
    <p class="muted small wide-row">Os valores ficam em texto no banco. Não use para senhas.</p>
    <p v-for="l in badEnv" :key="l.n" class="msg error small wide-row">Variável na linha {{ l.n }}: use CHAVE=valor.</p>

    <div class="wide-row field">
      <span class="field-label">Comando montado</span>
      <pre class="log">$ {{ preview || "…" }}</pre>
      <p v-if="previewError" class="msg warn small">{{ previewError }}</p>
    </div>

    <p v-if="error" class="msg error wide-row">{{ error }}</p>
    <div class="actions wide-row">
      <button type="submit" :disabled="saving || !!badEnv.length">{{ saving ? "Salvando…" : "Salvar gatilho" }}</button>
      <button type="button" class="secondary" @click="emit('cancel')">Cancelar</button>
    </div>
  </form>
</template>
