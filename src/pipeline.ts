// Pipeline do review em diagrama: cada agente aceso (rodou), apagado (não rodou, por regra ou
// porque a etapa anterior parou), em andamento ou em laranja (falhou), a partir do que a execução
// gravou: status, quality_status, audit_status, browser_status e as chamadas ao Claude.

import type { Run, RunCall } from "./api";

export type NodeState = "on" | "off" | "run" | "fail";

export interface PipeNode {
  key: string;
  name: string;
  state: NodeState;
  detail: string;
}

const callOf = (run: Run, name: string): RunCall | undefined => [...run.calls].reverse().find((c) => c.name === name);
const callState = (c: RunCall | undefined): NodeState | null =>
  !c ? null : c.status === "completed" ? "on" : c.status === "cancelled" ? "off" : "fail";
const callText = (c: RunCall) => (c.status === "cancelled" ? "cancelado" : c.status === "timed_out" ? "tempo esgotado" : "falhou");
const stageIs = (run: Run, word: string) => run.status === "running" && (run.stage ?? "").includes(word);
/** Status de etapa ('running') que ficou gravado numa execução que já terminou (cancelada,
 * falhou no meio ou o app fechou): a etapa foi interrompida. */
const live = (run: Run, status: string | null) => (status === "running" && run.status !== "running" ? "interrupted" : status);

export function pipelineNodes(run: Run): PipeNode[] {
  const bugs = run.findings.filter((f) => f.kind !== "melhoria");
  const improvements = run.findings.filter((f) => f.kind === "melhoria");
  const validated = run.findings.filter((f) => f.validation === "ok");
  const scored = improvements.filter((f) => f.score !== null);
  const audited = bugs.filter((f) => f.audit_decision);

  const sonnetCall = callOf(run, "review-sonnet");
  // sem a chamada gravada: execução antiga (conta pelos achados) ou parou antes do Sonnet
  const sonnetState: NodeState = stageIs(run, "Sonnet revisando")
    ? "run"
    : (callState(sonnetCall) ?? (run.findings.length || run.status === "completed" ? "on" : "off"));
  const sonnet: PipeNode = {
    key: "sonnet",
    name: "Sonnet",
    state: sonnetState,
    detail:
      sonnetState === "run"
        ? "revisando…"
        : sonnetCall && sonnetCall.status !== "completed"
          ? callText(sonnetCall)
          : sonnetState === "off"
            ? "não chegou a rodar"
            : run.status === "running" && !run.findings.length
              ? "concluído"
              : `${bugs.length} achado(s)`,
  };

  const validation: PipeNode = {
    key: "validation",
    name: "Validação",
    state: stageIs(run, "validando") ? "run" : run.findings.length || run.status === "completed" ? "on" : "off",
    detail: run.findings.length ? `${validated.length} de ${run.findings.length} ok` : run.status === "completed" ? "nada a validar" : "—",
  };

  const q = stageIs(run, "Haiku procurando") ? "running" : live(run, run.quality_status);
  const haiku: PipeNode = {
    key: "haiku",
    name: "Haiku",
    state: q === "running" ? "run" : q === "completed" ? "on" : q === "failed" || q === "interrupted" ? "fail" : "off",
    detail:
      q === "running"
        ? "procurando…"
        : q === "completed"
          ? `${improvements.length} melhoria(s)`
          : q === "failed"
            ? "falhou: segue só com bugs"
            : q === "interrupted"
              ? "interrompido"
              : q === "cancelled"
                ? "cancelado"
                : "não rodou",
  };

  const judgeCall = callOf(run, "judge-sonnet");
  const judge: PipeNode = {
    key: "judge",
    name: "Juiz",
    state: stageIs(run, "nota às melhorias") ? "run" : callState(judgeCall) ?? (scored.length ? "on" : "off"),
    detail: stageIs(run, "nota às melhorias")
      ? "dando nota…"
      : judgeCall && judgeCall.status !== "completed"
        ? `${callText(judgeCall)}: melhorias sem nota`
        : scored.length
          ? `${scored.length} nota(s)`
          : improvements.length
            ? "sem nota"
            : "sem melhorias",
  };

  const a = stageIs(run, "Opus auditando") ? "running" : live(run, run.audit_status);
  const opus: PipeNode = {
    key: "opus",
    name: "Opus",
    state: a === "running" ? "run" : a === "completed" ? "on" : a === "failed" || a === "interrupted" ? "fail" : "off",
    detail:
      a === "running"
        ? "auditando…"
        : a === "completed"
          ? `${audited.length} auditado(s)`
          : a === "failed"
            ? "falhou"
            : a === "interrupted"
              ? "interrompido"
              : a === "cancelled"
                ? "cancelado"
                : a === "not_needed"
                  ? "sem bugs válidos"
                  : "não rodou",
  };

  const nodes = [sonnet, validation, haiku, judge, opus];
  // pedido de mudanças: só em PR do GitHub (nulo quando não se aplica)
  const r = stageIs(run, "pedido de mudanças") ? "running" : live(run, run.rc_status);
  if (r) {
    const count = (d: string) => run.findings.filter((f) => f.rc_decision === d).length;
    nodes.push({
      key: "rc",
      name: "RC",
      state: r === "running" ? "run" : r === "completed" ? "on" : r === "failed" || r === "interrupted" ? "fail" : "off",
      detail:
        r === "running"
          ? "validando…"
          : r === "completed"
            ? `${count("bloqueante")} bloqueante(s), ${count("sugestao")} sugestão(ões)`
            : r === "failed"
              ? "falhou: use a seleção manual"
              : r === "interrupted"
                ? "interrompido"
                : r === "not_needed"
                  ? "nada a pedir"
                  : "cancelado",
    });
  }
  const b = live(run, run.browser_status);
  if (b) {
    const ok = run.browser_tests.filter((t) => t.status === "aprovado").length;
    nodes.push({
      key: "browser",
      name: "Navegador",
      state: b === "running" ? "run" : b === "completed" ? "on" : b === "failed" || b === "interrupted" ? "fail" : "off",
      detail:
        b === "running"
          ? "testando…"
          : b === "completed"
            ? `${ok} de ${run.browser_tests.length} aprovado(s)`
            : b === "blocked"
              ? "bloqueado"
              : b === "failed"
                ? "falhou"
                : b === "interrupted"
                  ? "interrompido"
                  : "cancelado",
    });
  }
  return nodes;
}
