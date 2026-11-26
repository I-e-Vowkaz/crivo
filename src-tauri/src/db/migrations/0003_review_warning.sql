-- Aviso de qualidade do review (ex.: feito sem o subagente branch-reviewer).
ALTER TABLE review_runs ADD COLUMN warning TEXT;
