<script setup lang="ts">
// O pipeline do review numa linha: cada agente aceso, apagado, rodando ou em laranja.
import { computed } from "vue";
import type { Run } from "../api";
import { pipelineNodes } from "../pipeline";

const props = defineProps<{ run: Run }>();
const nodes = computed(() => pipelineNodes(props.run));
const stateText = { on: "rodou", off: "não rodou", run: "rodando", fail: "falhou" } as const;
</script>

<template>
  <div class="pipe" role="list" aria-label="Pipeline do review">
    <template v-for="(n, i) in nodes" :key="n.key">
      <span v-if="i > 0" :class="['pipe-link', { on: n.state === 'on' || n.state === 'run' }]" aria-hidden="true" />
      <div :class="['pipe-node', n.state]" role="listitem" :aria-label="`${n.name}: ${stateText[n.state]}, ${n.detail}`">
        <span class="pipe-name">{{ n.name }}</span>
        <span class="pipe-detail">{{ n.detail }}</span>
      </div>
    </template>
  </div>
</template>
