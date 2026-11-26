<script setup lang="ts">
import { computed } from "vue";
import { diffLines } from "../diff";
import { diffMode, setDiffMode } from "../diffMode";
import type { FixEdit } from "../api";
import DiffRows from "./DiffRows.vue";

const props = defineProps<{ edit: FixEdit; applied?: boolean }>();

const isNew = computed(() => props.edit.line === 0 && !props.edit.existing_code.trim());
const rows = computed(() => diffLines(props.edit.existing_code, props.edit.improved_code, Math.max(props.edit.line, 1)));
const range = computed(() =>
  isNew.value ? "arquivo novo" : props.edit.end_line > props.edit.line ? `${props.edit.line}-${props.edit.end_line}` : `${props.edit.line}`,
);
</script>

<template>
  <div class="diff">
    <div class="diff-head">
      <code>{{ edit.file }}</code>
      <span class="muted">:{{ range }}</span>
      <span v-if="applied" class="badge ok">aplicado</span>
      <span class="diff-mode" role="group" aria-label="Modo do diff">
        <button type="button" :class="['icon-btn', { on: diffMode === 'split' }]" :aria-pressed="diffMode === 'split'" @click="setDiffMode('split')">
          lado a lado
        </button>
        <button type="button" :class="['icon-btn', { on: diffMode === 'unified' }]" :aria-pressed="diffMode === 'unified'" @click="setDiffMode('unified')">
          unificado
        </button>
      </span>
    </div>
    <DiffRows :rows="rows" />
  </div>
</template>
