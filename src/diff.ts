import type { Hunk, HunkLine } from "./api";

// Diff de linhas entre o trecho atual e a sugestão, para mostrar como no `git diff`:
// linhas removidas e adicionadas, com o trecho que mudou dentro da linha destacado.

export interface Seg {
  text: string;
  /** Parte que mudou em relação à linha par (removida ↔ adicionada). */
  hl: boolean;
}

export interface DiffLine {
  /** `hunk`: separador `@@ … @@` entre trechos (sem números). */
  kind: "ctx" | "del" | "add" | "hunk";
  oldNo: number | null;
  newNo: number | null;
  segs: Seg[];
}

/** Selo de um achado na linha do diff (aba "Diff"). */
export interface DiffMark {
  seq: number;
  /** Severidade do bug ou "melhoria". */
  label: string;
  /** Classe `.sev` existente: critical, high, medium, low ou melhoria. */
  cls: string;
  title: string;
}

const lines = (s: string) => {
  const t = s.replace(/\r/g, "").replace(/^\n+|\n+$/g, "");
  return t.trim() ? t.split("\n") : [];
};

/** Maior subsequência comum de linhas → operações em ordem. Blocos grandes demais viram troca inteira. */
function ops(a: string[], b: string[]): ("=" | "-" | "+")[] {
  const n = a.length;
  const m = b.length;
  if (n * m > 250_000) return [...a.map(() => "-" as const), ...b.map(() => "+" as const)];
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out: ("=" | "-" | "+")[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push("=");
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push("-");
      i++;
    } else {
      out.push("+");
      j++;
    }
  }
  while (i++ < n) out.push("-");
  while (j++ < m) out.push("+");
  return out;
}

/** Destaca o meio que difere entre duas linhas (prefixo e sufixo comuns ficam sem destaque). */
function pair(a: string, b: string): [Seg[], Seg[]] {
  let p = 0;
  while (p < a.length && p < b.length && a[p] === b[p]) p++;
  let s = 0;
  while (s < a.length - p && s < b.length - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  const split = (x: string): Seg[] =>
    [
      { text: x.slice(0, p), hl: false },
      { text: x.slice(p, x.length - s), hl: true },
      { text: x.slice(x.length - s), hl: false },
    ].filter((g) => g.text);
  return [split(a), split(b)];
}

export function diffLines(oldCode: string, newCode: string, start: number): DiffLine[] {
  const a = lines(oldCode);
  const b = lines(newCode);
  const out: DiffLine[] = [];
  let oi = 0;
  let ni = 0;
  const seq = ops(a, b);
  for (let k = 0; k < seq.length; ) {
    if (seq[k] === "=") {
      out.push({ kind: "ctx", oldNo: start + oi, newNo: start + ni, segs: [{ text: a[oi], hl: false }] });
      oi++;
      ni++;
      k++;
      continue;
    }
    // um trecho de removidas seguido de adicionadas: pareia linha a linha para o destaque
    const dels: number[] = [];
    const adds: number[] = [];
    while (k < seq.length && seq[k] !== "=") {
      if (seq[k] === "-") dels.push(oi++);
      else adds.push(ni++);
      k++;
    }
    const segs = (x: string): Seg[] => [{ text: x, hl: false }];
    const delSegs = dels.map((i) => segs(a[i]));
    const addSegs = adds.map((i) => segs(b[i]));
    for (let x = 0; x < Math.min(dels.length, adds.length); x++) [delSegs[x], addSegs[x]] = pair(a[dels[x]], b[adds[x]]);
    dels.forEach((i, x) => out.push({ kind: "del", oldNo: start + i, newNo: null, segs: delSegs[x] }));
    adds.forEach((i, x) => out.push({ kind: "add", oldNo: null, newNo: start + i, segs: addSegs[x] }));
  }
  return out;
}

/** Linha do diff lado a lado: o atual à esquerda e o proposto à direita (null = lado vazio). */
export interface SplitRow {
  left: DiffLine | null;
  right: DiffLine | null;
}

/** Pareia as linhas para o modo lado a lado: contexto nos dois lados; cada bloco de removidas
 * fica ao lado do bloco de adicionadas que vem em seguida, linha a linha. */
export function splitRows(rows: DiffLine[]): SplitRow[] {
  const out: SplitRow[] = [];
  let i = 0;
  while (i < rows.length) {
    if (rows[i].kind === "ctx" || rows[i].kind === "hunk") {
      out.push({ left: rows[i], right: rows[i] });
      i++;
      continue;
    }
    const dels: DiffLine[] = [];
    const adds: DiffLine[] = [];
    while (i < rows.length && rows[i].kind === "del") dels.push(rows[i++]);
    while (i < rows.length && rows[i].kind === "add") adds.push(rows[i++]);
    for (let k = 0; k < Math.max(dels.length, adds.length); k++) out.push({ left: dels[k] ?? null, right: adds[k] ?? null });
  }
  return out;
}

/** Trechos do `git diff` (já numerados pelo backend) → linhas, com um separador por trecho e o
 * destaque dentro da linha em cada bloco de removidas seguido de adicionadas. */
export function hunkRows(hunks: Hunk[]): DiffLine[] {
  const out: DiffLine[] = [];
  const plain = (t: string): Seg[] => [{ text: t, hl: false }];
  for (const h of hunks) {
    out.push({ kind: "hunk", oldNo: null, newNo: null, segs: plain(h.header) });
    const ls = h.lines;
    let i = 0;
    while (i < ls.length) {
      if (ls[i].kind === "ctx") {
        out.push({ kind: "ctx", oldNo: ls[i].old_no, newNo: ls[i].new_no, segs: plain(ls[i].text) });
        i++;
        continue;
      }
      const dels: HunkLine[] = [];
      const adds: HunkLine[] = [];
      while (i < ls.length && ls[i].kind === "del") dels.push(ls[i++]);
      while (i < ls.length && ls[i].kind === "add") adds.push(ls[i++]);
      const delSegs = dels.map((l) => plain(l.text));
      const addSegs = adds.map((l) => plain(l.text));
      for (let x = 0; x < Math.min(dels.length, adds.length); x++) [delSegs[x], addSegs[x]] = pair(dels[x].text, adds[x].text);
      dels.forEach((l, x) => out.push({ kind: "del", oldNo: l.old_no, newNo: null, segs: delSegs[x] }));
      adds.forEach((l, x) => out.push({ kind: "add", oldNo: null, newNo: l.new_no, segs: addSegs[x] }));
    }
  }
  return out;
}
