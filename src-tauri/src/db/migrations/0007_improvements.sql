-- Melhorias técnicas (complexidade desnecessária), do agente complexity-reviewer (Haiku).
-- Ficam na tabela findings com kind = 'melhoria' e sem severidade; não passam pelo Opus.
ALTER TABLE findings ADD COLUMN category TEXT;
ALTER TABLE findings ADD COLUMN evidence TEXT;
ALTER TABLE findings ADD COLUMN suggestion TEXT;

ALTER TABLE review_runs ADD COLUMN quality_status TEXT
    CHECK (quality_status IN ('running', 'completed', 'failed', 'cancelled'));
