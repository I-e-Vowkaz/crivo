-- Melhorias no formato do Qodo: trecho literal (conferido pelo app contra o arquivo),
-- substituição completa e nota do juiz (Sonnet), de 0 a 10, com a importância derivada dela.
-- A coluna suggestion (0007) fica só para as melhorias gravadas antes desta migration.
ALTER TABLE findings ADD COLUMN end_line INTEGER;
ALTER TABLE findings ADD COLUMN existing_code TEXT;
ALTER TABLE findings ADD COLUMN improved_code TEXT;
ALTER TABLE findings ADD COLUMN score INTEGER;
ALTER TABLE findings ADD COLUMN score_why TEXT;
ALTER TABLE findings ADD COLUMN importance TEXT CHECK (importance IN ('alta', 'media', 'baixa'));
