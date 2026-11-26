<script setup lang="ts">
// Linhas de um diff, lado a lado ou unificado (conforme `diffMode`), com os selos dos achados
// nas linhas do lado novo. Usado pelo DiffView (sugestões) e pela aba "Diff" do painel.
import { computed } from "vue";
import { splitRows, type DiffLine, type DiffMark } from "../diff";
import { diffMode } from "../diffMode";

const props = withDefaults(
  defineProps<{
    rows: DiffLine[];
    /** Selos por número da linha no lado novo. */
    marks?: Record<number, DiffMark[]>;
    leftLabel?: string;
    rightLabel?: string;
  }>(),
  { marks: () => ({}), leftLabel: "atual", rightLabel: "proposto" },
);
const emit = defineEmits<{ mark: [seq: number] }>();

const pairs = computed(() => splitRows(props.rows));
const marksOf = (r: DiffLine | null) => (r && r.kind !== "del" && r.newNo !== null ? (props.marks[r.newNo] ?? []) : []);
</script>

<template>
  <div v-if="diffMode === 'split' && rows.length" class="diff-split">
    <div class="diff-side del-side">
      <div class="diff-side-head">{{ leftLabel }}</div>
      <div class="diff-body">
        <div v-for="(p, i) in pairs" :key="i" :class="['diff-line', 'one', p.left?.kind ?? 'gap']">
          <template v-if="p.left?.kind === 'hunk'">
            <span class="hunk-text">{{ p.left.segs[0]?.text }}</span>
          </template>
          <template v-else>
            <span class="ln">{{ p.left?.oldNo ?? "" }}</span>
            <span class="sign">{{ p.left?.kind === "del" ? "-" : " " }}</span>
            <span class="code"><span v-for="(s, j) in p.left?.segs ?? []" :key="j" :class="{ hl: s.hl }">{{ s.text }}</span></span>
          </template>
        </div>
      </div>
    </div>
    <div class="diff-side add-side">
      <div class="diff-side-head">{{ rightLabel }}</div>
      <div class="diff-body">
        <div v-for="(p, i) in pairs" :key="i" :class="['diff-line', 'one', p.right?.kind ?? 'gap']">
          <template v-if="p.right?.kind === 'hunk'">
            <span class="hunk-text">&nbsp;</span>
          </template>
          <template v-else>
            <span class="ln">{{ p.right?.newNo ?? "" }}</span>
            <span class="sign">{{ p.right?.kind === "add" ? "+" : " " }}</span>
            <span class="code"
              ><span v-for="(s, j) in p.right?.segs ?? []" :key="j" :class="{ hl: s.hl }">{{ s.text }}</span
              ><button
                v-for="m in marksOf(p.right)"
                :key="m.seq"
                type="button"
                :class="['sev', 'diff-mark', m.cls]"
                :title="m.title"
                @click="emit('mark', m.seq)"
              >
                {{ m.label }}
              </button></span
            >
          </template>
        </div>
      </div>
    </div>
  </div>

  <div v-else class="diff-body">
    <div v-for="(r, i) in rows" :key="i" :class="['diff-line', r.kind]">
      <template v-if="r.kind === 'hunk'">
        <span class="hunk-text">{{ r.segs[0]?.text }}</span>
      </template>
      <template v-else>
        <span class="ln">{{ r.oldNo ?? "" }}</span>
        <span class="ln">{{ r.newNo ?? "" }}</span>
        <span class="sign">{{ r.kind === "del" ? "-" : r.kind === "add" ? "+" : " " }}</span>
        <span class="code"
          ><span v-for="(s, j) in r.segs" :key="j" :class="{ hl: s.hl }">{{ s.text }}</span
          ><button
            v-for="m in marksOf(r)"
            :key="m.seq"
            type="button"
            :class="['sev', 'diff-mark', m.cls]"
            :title="m.title"
            @click="emit('mark', m.seq)"
          >
            {{ m.label }}
          </button></span
        >
      </template>
    </div>
    <div v-if="!rows.length" class="diff-line ctx"><span class="ln" /><span class="ln" /><span class="sign" /><span class="code muted">(sem conteúdo)</span></div>
  </div>
</template>
