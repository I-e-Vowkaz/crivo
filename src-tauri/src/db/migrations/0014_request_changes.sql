-- Pedido de mudanças (RC) montado pelo agente rc-validator no fim do review de PRs do GitHub.
--
-- review_runs.rc_status: running, completed, failed, cancelled ou not_needed (sem candidatos);
-- nulo quando a etapa não se aplica (Bitbucket, branch local, execuções antigas).
-- review_runs.rc_summary: corpo do review; rc_sent_url: link do review enviado.
-- findings.rc_decision: bloqueante, sugestao ou fora; rc_duplicate_of: seq do achado principal
-- quando este repete outro; rc_body: o comentário pronto; rc_reason: por que (só para o revisor).
ALTER TABLE review_runs ADD COLUMN rc_status TEXT;
ALTER TABLE review_runs ADD COLUMN rc_summary TEXT;
ALTER TABLE review_runs ADD COLUMN rc_sent_url TEXT;
ALTER TABLE findings ADD COLUMN rc_decision TEXT CHECK (rc_decision IN ('bloqueante', 'sugestao', 'fora'));
ALTER TABLE findings ADD COLUMN rc_duplicate_of INTEGER;
ALTER TABLE findings ADD COLUMN rc_body TEXT;
ALTER TABLE findings ADD COLUMN rc_reason TEXT;
