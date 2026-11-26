// Chamadas ao backend Rust. Os tipos espelham `src-tauri/src/commands.rs`.
// Nenhuma chamada devolve segredo: só se ele está salvo.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ref } from "vue";

export type SecretKind = "bitbucket" | "jira" | "github";

export interface AppInfo {
  version: string;
  db_path: string;
  schema_version: number;
  schema_latest: number;
  keyring_error: string | null;
}

export interface BitbucketAccount {
  user: { uuid: string; display_name: string; account_id: string | null };
  scopes: string[];
  workspace: string;
  workspace_repos: number | null;
  validated_at: string | null;
}

export interface SecretStatus {
  kind: SecretKind;
  saved: boolean;
}

export interface Reviewer {
  name: string;
  uuid: string | null;
  approved: boolean;
  state: "approved" | "changes_requested" | null;
}

export interface LastRun {
  id: string;
  status: RunStatus;
  stage: string | null;
  started_at: string;
  findings: number;
  high: number;
  /** Melhorias técnicas válidas (fora de `findings`). */
  improvements: number;
  warning: string | null;
  outdated: boolean;
}

export interface Pr {
  id: number;
  repo: string;
  provider: "bitbucket" | "github";
  owner: string;
  bb_id: number;
  title: string;
  url: string;
  author: string | null;
  category: "review_requested" | "authored" | "both";
  changes_requested: boolean;
  changed_since_sync: boolean;
  approved_by_me: boolean;
  reviewed_sha: string | null;
  needs_rereview: boolean;
  /** Início do último review concluído nos mesmos commits de origem e destino (revisar de novo repete o trabalho). */
  same_commits_reviewed_at: string | null;
  reviewers: Reviewer[];
  source_branch: string;
  dest_branch: string;
  updated_on: string;
  last_run: LastRun | null;
  /** Comentários no PR, como vem da listagem do Bitbucket (inclui os meus). */
  comment_count: number | null;
  /** Última análise dos comentários (só nos meus PRs). */
  fix: FixSummary | null;
}

/** `committed`: só na branch local (commit feito e branch avançada, push ainda não). */
export type FixStatus = "analyzing" | "ready" | "failed" | "cancelled" | "committing" | "committed" | "pushed";
/** `new`: comentário ainda não analisado (Meus PRs: o usuário escolhe quais o Claude analisa). */
export type FixCommentStatus = "new" | "queued" | "analyzing" | "procede" | "nao_procede" | "resposta" | "applied" | "failed";

export interface FixSummary {
  id: string;
  status: FixStatus;
  total: number;
  /** Sugestões esperando aprovação. */
  pending: number;
  applied: number;
  /** Achados do review que estão na correção: [run_id, seq do achado, status do item]. */
  findings: [string, number, FixCommentStatus][];
}

export interface FixEdit {
  file: string;
  line: number;
  end_line: number;
  existing_code: string;
  improved_code: string;
}

export interface FixComment {
  id: number;
  seq: number;
  /** Origem: comentário do Bitbucket (Meus PRs) ou achado do review (Minhas branches). */
  bb_comment_id: number | null;
  finding_run_id: string | null;
  finding_seq: number | null;
  author: string | null;
  body: string;
  path: string | null;
  line: number | null;
  outdated: boolean;
  url: string | null;
  created_on: string | null;
  replies: { author: string; body: string; created_on: string | null }[];
  status: FixCommentStatus;
  explanation: string | null;
  edits: FixEdit[];
  error: string | null;
  applied_at: string | null;
  messages: ChatMessage[];
}

export interface FixSession {
  id: string;
  kind: "pr" | "branch";
  pr_id: number | null;
  repo_slug: string | null;
  branch: string | null;
  base_branch: string | null;
  status: FixStatus;
  stage: string | null;
  error: string | null;
  cost_usd: number;
  base_sha: string | null;
  card_key: string | null;
  commit_sha: string | null;
  commit_subject: string | null;
  pushed_at: string | null;
  created_at: string;
  run_dir: string;
  busy: boolean;
  /** Rodando pelo terminal (`pr-review-orchestrator comments`): o painel relê a cada 3 s. */
  terminal: boolean;
  comments: FixComment[];
}

export interface PrList {
  prs: Pr[];
  last_sync: string | null;
}

export interface SyncResult {
  user: string;
  repos: { slug: string; open: number; error: string | null; fetch_error: string | null }[];
  requests: number;
}

export type RunStatus = "running" | "completed" | "failed" | "cancelled";

export interface Finding {
  seq: number;
  project: string | null;
  kind: "bug" | "padrao" | "requisito" | "melhoria" | null;
  severity: "critical" | "high" | "medium" | "low" | null;
  severity_reason: string | null;
  impact: string | null;
  file: string | null;
  line: number | null;
  title: string | null;
  description: string | null;
  steps: string[];
  pattern_ref: string | null;
  confidence: "confirmed" | "plausible" | null;
  markdown: string | null;
  validation: "ok" | "rejected";
  reject_reason: string | null;
  in_diff: boolean | null;
  audit_decision: "confirmado" | "severidade_ajustada" | "falso_positivo" | "evidencia_insuficiente" | null;
  audit_new_severity: string | null;
  audit_rationale: string | null;
  /** Só em melhoria técnica (kind = "melhoria"). */
  category: "redundante" | "codigo_morto" | "simplificavel" | "duplicado" | "abstracao_desnecessaria" | "desempenho" | "teste" | null;
  evidence: string | null;
  /** Forma livre das melhorias gravadas antes da migration 0008. */
  suggestion: string | null;
  end_line: number | null;
  existing_code: string | null;
  improved_code: string | null;
  /** Nota do juiz (Sonnet), de 0 a 10; null se o juiz falhou. */
  score: number | null;
  score_why: string | null;
  importance: "alta" | "media" | "baixa" | null;
  /** Pedido de mudanças (rc-validator): o que entra no RC e o comentário pronto. */
  rc_decision: "bloqueante" | "sugestao" | "fora" | null;
  /** Achado principal, quando este repete outro. */
  rc_duplicate_of: number | null;
  rc_body: string | null;
  /** Por que o agente decidiu assim (não vai para a PR). */
  rc_reason: string | null;
}

export interface Run {
  id: string;
  /** Nulo no review de branch local (`repo_slug` + `branch`). */
  pr_id: number | null;
  repo_slug: string | null;
  branch: string | null;
  status: RunStatus;
  stage: string | null;
  audit_status: "not_needed" | "running" | "completed" | "failed" | "cancelled" | null;
  /** Análise de complexidade desnecessária (Haiku). */
  quality_status: "running" | "completed" | "failed" | "cancelled" | null;
  card_key: string | null;
  started_at: string;
  finished_at: string | null;
  cost_usd: number | null;
  error: string | null;
  warning: string | null;
  browser_status: "blocked" | "running" | "completed" | "failed" | "cancelled" | null;
  browser_note: string | null;
  browser_checkout_pr_id: number | null;
  browser_checkout_label: string | null;
  /** Pedido de mudanças montado pelo rc-validator (só PR do GitHub); nulo quando não se aplica. */
  rc_status: "running" | "completed" | "failed" | "cancelled" | "not_needed" | null;
  rc_summary: string | null;
  /** Link do review enviado. */
  rc_sent_url: string | null;
  run_dir: string;
  findings: Finding[];
  browser_tests: BrowserTest[];
  log: string[];
  /** Etapas com hora (vazio nas execuções anteriores a 24/11/2026). */
  steps: RunStep[];
  /** Chamadas ao Claude, com modelo, custo e duração. */
  calls: RunCall[];
}

export interface RunStep {
  seq: number;
  label: string;
  started_at: string;
  finished_at: string | null;
  cost_usd: number;
  /** Agentes que rodaram na etapa (`review-sonnet`, `audit-opus`…). */
  calls: string[];
  failed: boolean;
}

export interface RunCall {
  name: string;
  model: "sonnet" | "haiku" | "opus" | null;
  status: "completed" | "failed" | "cancelled" | "timed_out";
  cost_usd: number;
  duration_ms: number | null;
  turns: number | null;
  step_seq: number | null;
  /** Lida dos arquivos de uma execução antiga (sem etapa nem hora). */
  imported: boolean;
}

export interface AgentUsage {
  name: string;
  model: string | null;
  calls: number;
  failed: number;
  cost_total: number;
  cost_avg: number;
  duration_avg_ms: number | null;
}

export interface Usage {
  runs_completed: number;
  cost_avg: number;
  cost_min: number;
  cost_max: number;
  cost_total: number;
  agents: AgentUsage[];
  /** Os 30 reviews concluídos mais recentes, do mais antigo ao mais novo. */
  recent: { id: string; started_at: string; cost_usd: number; label: string }[];
}

export interface BrowserTest {
  seq: number;
  title: string | null;
  url: string | null;
  preconditions: string | null;
  actions: string[];
  expected: string | null;
  observed: string | null;
  evidence: string | null;
  status: "aprovado" | "reprovado" | "bloqueado" | "nao_executado";
  finding_ref: string | null;
}

export interface TestApp {
  id: string;
  name: string;
  base_url: string;
  login_url: string;
  username: string;
  allowed_hosts: string[];
  repos: string[];
}

export interface TestAppView extends TestApp {
  password_saved: boolean;
  effective_hosts: string[];
}

export interface CheckoutResult {
  repo: string;
  branch: string;
  head: string;
  at_pr_commit: boolean;
}

export interface ChatMessage {
  id: number;
  role: "user" | "assistant" | "error";
  content: string;
  cost_usd: number | null;
  created_at: string;
}

/** Aba "Diff": o commit revisado ou a ponta atual da branch (último sync). */
export type DiffViewKind = "reviewed" | "current";

export interface DiffFile {
  path: string;
  /** Caminho antigo, só em renomeação. */
  old_path: string | null;
  status: "added" | "deleted" | "renamed" | "modified";
  added: number;
  removed: number;
  binary: boolean;
}

export interface HunkLine {
  kind: "ctx" | "del" | "add";
  old_no: number | null;
  new_no: number | null;
  text: string;
}

export interface Hunk {
  /** A linha `@@ … @@` inteira. */
  header: string;
  lines: HunkLine[];
}

export interface FileDiff {
  hunks: Hunk[];
  binary: boolean;
  /** Diff cortado em 1 MB. */
  truncated: boolean;
}

export interface DiffSummary {
  base_sha: string;
  head_sha: string;
  reviewed_sha: string;
  /** Commits na ponta depois do revisado (só na visão atual). */
  ahead: number | null;
  same_as_reviewed: boolean;
  /** Motivo de não haver diff (commit ou branch fora do clone). */
  missing: string | null;
  files: DiffFile[];
}

export interface LimitState {
  message: string;
  reset_at: number | null;
  reset_text: string | null;
  since: number;
  retry_at: number;
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  secretStatus: (kind: SecretKind) => invoke<SecretStatus>("secret_status", { kind }),
  secretDelete: (kind: SecretKind) => invoke<SecretStatus>("secret_delete", { kind }),
  settingGet: <T = unknown>(key: string) => invoke<T | null>("setting_get", { key }),
  settingSet: (key: string, value: unknown) => invoke<void>("setting_set", { key, value }),
  /** Valida o token contra a API e só então salva. */
  bitbucketSaveToken: (value: string) => invoke<BitbucketAccount>("bitbucket_save_token", { value }),
  bitbucketValidate: () => invoke<BitbucketAccount>("bitbucket_validate"),
  bitbucketAccount: () => invoke<BitbucketAccount | null>("bitbucket_account"),
  githubSaveToken: (value: string) => invoke<{login: string; mode: string}>("github_save_token", { value }),
  githubUseGh: () => invoke<{login: string; mode: string}>("github_use_gh"),
  githubLogin: () => invoke<void>("github_login"),
  syncPrs: () => invoke<SyncResult>("sync_prs"),
  listPrs: () => invoke<PrList>("list_prs"),
  usageGet: () => invoke<Usage>("usage_get"),
  triggersList: () => invoke<TriggerView[]>("triggers_list"),
  triggerSave: (trigger: Trigger) => invoke<Trigger>("trigger_save", { trigger }),
  triggerDelete: (id: string) => invoke<void>("trigger_delete", { id }),
  triggerStart: (id: string) => invoke<void>("trigger_start", { id }),
  triggerStop: (id: string) => invoke<void>("trigger_stop", { id }),
  triggerRestart: (id: string) => invoke<void>("trigger_restart", { id }),
  triggerLog: (id: string) => invoke<string[]>("trigger_log", { id }),
  triggerScripts: (packageJson: string) => invoke<string[]>("trigger_scripts", { packageJson }),
  nodeRuntimes: () => invoke<NodeRuntime[]>("node_runtimes"),
  triggerPreview: (trigger: Trigger) => invoke<string>("trigger_preview", { trigger }),
  reviewStart: (prId: number) => invoke<string>("review_start", { prId }),
  reviewCancel: (runId: string) => invoke<void>("review_cancel", { runId }),
  reviewGet: (runId: string) => invoke<Run>("review_get", { runId }),
  publishFindings: (runId: string, seqs: number[], requestChanges: boolean) => invoke<{seq: number; url: string | null}[]>("publish_findings", { runId, seqs, requestChanges }),
  sendRequestChanges: (runId: string, skip: number[]) =>
    invoke<{ url: string | null; event: "REQUEST_CHANGES" | "COMMENT"; comments: number }>("send_request_changes", { runId, skip }),
  testAppsGet: () => invoke<TestAppView[]>("test_apps_get"),
  testAppSave: (apps: TestApp[], passwordFor?: string, password?: string) =>
    invoke<TestAppView[]>("test_app_save", { apps, passwordFor, password }),
  /** fetch → checkout → pull --ff-only no clone local (recusa se houver alteração sem commit). */
  checkoutPr: (prId: number) => invoke<CheckoutResult>("checkout_pr", { prId }),
  chatAsk: (runId: string, question: string) => invoke<void>("chat_ask", { runId, question }),
  chatGet: (runId: string) => invoke<{ messages: ChatMessage[]; busy: boolean }>("chat_get", { runId }),
  chatCancel: (runId: string) => invoke<void>("chat_cancel", { runId }),
  diffFiles: (runId: string, view: DiffViewKind) => invoke<DiffSummary>("diff_files", { runId, view }),
  /** Um arquivo, preso aos commits do resumo (`base_sha`/`head_sha`) que a tela mostra. */
  diffFile: (runId: string, view: DiffViewKind, baseSha: string, headSha: string, path: string, oldPath: string | null) =>
    invoke<FileDiff>("diff_file", { runId, view, baseSha, headSha, path, oldPath }),
  claudeLimitGet: () => invoke<LimitState | null>("claude_limit_get"),
  claudeLimitClear: () => invoke<void>("claude_limit_clear"),
  /** Última análise dos comentários do PR (null se nunca rodou). */
  fixGet: (prId: number) => invoke<FixSession | null>("fix_get", { prId }),
  /** Última correção da branch local (null se nunca rodou). */
  fixGetBranch: (repoSlug: string, branch: string) => invoke<FixSession | null>("fix_get_branch", { repoSlug, branch }),
  /** "Corrigir" em achados de um review de branch local; devolve o id da correção. */
  fixAddFindings: (runId: string, seqs: number[]) => invoke<string>("fix_add_findings", { runId, seqs }),
  fixDiscard: (fixId: string) => invoke<void>("fix_discard", { fixId }),
  /** "Buscar novos comentários": acrescenta as threads que chegaram depois (sem o Claude). */
  fixRefresh: (fixId: string) => invoke<void>("fix_refresh", { fixId }),
  /** "Analisar" um ou mais comentários (ou de novo, depois de falhar); a fila roda um por vez. */
  fixAnalyze: (commentIds: number[]) => invoke<void>("fix_analyze", { commentIds }),
  /** `force`: descarta as sugestões aprovadas e ainda sem commit da análise anterior. */
  fixStart: (prId: number, force = false) => invoke<string>("fix_start", { prId, force }),
  fixCancel: (fixId: string) => invoke<void>("fix_cancel", { fixId }),
  fixAsk: (commentId: number, text: string) => invoke<void>("fix_ask", { commentId, text }),
  /** Grava a sugestão no worktree da correção; devolve os arquivos alterados. */
  fixApprove: (commentId: number) => invoke<string[]>("fix_approve", { commentId }),
  /** Commit pelo committer; no PR já faz o push; na branch local avança a branch. */
  fixCommit: (fixId: string) => invoke<void>("fix_commit", { fixId }),
  /** Push da branch local depois do commit. */
  fixPush: (fixId: string) => invoke<void>("fix_push", { fixId }),
  /** Repositórios da pasta configurada e as branches locais (sem rede). */
  localBranches: () => invoke<LocalProject[]>("local_branches"),
  reviewStartBranch: (repoSlug: string, branch: string) => invoke<string>("review_start_branch", { repoSlug, branch }),
};

export interface LocalBranch {
  name: string;
  date: string;
  sha: string;
  subject: string;
  upstream: string | null;
  /** Diferença para a remota, como o git escreve: `[ahead 2]`, `[gone]`. */
  track: string;
  /** Aberta no checkout principal do clone. */
  head: boolean;
  last_run: LastRun | null;
  fix: FixSummary | null;
}

export interface LocalProject {
  slug: string;
  kind: "back" | "front" | "outro";
  base: string | null;
  last_commit: string | null;
  error: string | null;
  branches: LocalBranch[];
}

export type TriggerKind = "npm" | "shell";

export interface Trigger {
  id: string;
  name: string;
  kind: TriggerKind;
  package_json: string;
  command: string;
  scripts: string[];
  package_manager: string;
  node: string;
  node_options: string;
  workdir: string;
  script: string;
  stop_script: string;
  args: string;
  env: [string, string][];
}

export type TriggerState =
  | { state: "stopped" }
  | { state: "running"; since: string; pgid: number }
  | { state: "stopping" }
  | { state: "exited"; code: number | null; at: string }
  | { state: "failed"; message: string };

export interface TriggerView {
  trigger: Trigger;
  state: TriggerState;
  preview: string;
  error: string | null;
}

export interface NodeRuntime {
  /** Vazio = o do sistema (PATH). */
  path: string;
  version: string;
  source: string;
}

export const events = {
  onTriggerStatus: (cb: (e: { id: string; state: TriggerState }) => void): Promise<UnlistenFn> =>
    listen("trigger-status", (ev) => cb(ev.payload as never)),
  onTriggerLog: (cb: (e: { id: string; lines: string[] }) => void): Promise<UnlistenFn> =>
    listen("trigger-log", (ev) => cb(ev.payload as never)),
  onReviewUpdate: (cb: (e: { run_id: string; pr_id: number; status: RunStatus; stage: string }) => void): Promise<UnlistenFn> =>
    listen("review-update", (ev) => cb(ev.payload as never)),
  onReviewLog: (cb: (e: { run_id: string; line: string }) => void): Promise<UnlistenFn> =>
    listen("review-log", (ev) => cb(ev.payload as never)),
  onClaudeLimit: (cb: (e: LimitState | null) => void): Promise<UnlistenFn> =>
    listen("claude-limit", (ev) => cb(ev.payload as LimitState | null)),
  onChatUpdate: (cb: (e: { run_id: string; line: string | null; busy: boolean }) => void): Promise<UnlistenFn> =>
    listen("chat-update", (ev) => cb(ev.payload as never)),
  onFixUpdate: (
    cb: (e: { fix_id: string; pr_id: number; repo_slug: string; branch: string; line: string | null; busy: boolean }) => void,
  ): Promise<UnlistenFn> =>
    listen("fix-update", (ev) => cb(ev.payload as never)),
};

/** Erros do backend chegam como texto. */
export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

export const formatDate = (iso: string | null) => (iso ? new Date(iso).toLocaleString("pt-BR") : "—");

/** Site do Jira (`jira.site`), sem protocolo nem `/` no fim; vazio enquanto não for configurado. */
export const jiraSite = ref("");

export const normalizeJiraSite = (s: string) => s.trim().replace(/^https?:\/\//, "").replace(/\/+$/, "");

export async function loadJiraSite() {
  jiraSite.value = normalizeJiraSite((await api.settingGet<string>("jira.site")) ?? "");
}

/** Card do Jira da branch: o nome da branch é a chave do card (`PROJ-123`). `null` sem site configurado. */
export const cardUrl = (branch: string) =>
  jiraSite.value ? `https://${jiraSite.value}/browse/${encodeURIComponent(branch)}` : null;
