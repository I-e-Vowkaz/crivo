-- PR (pull_requests.id) que precisa de checkout para o teste no navegador rodar
-- (o próprio PR ou o do outro lado, back/front, do mesmo card).
ALTER TABLE review_runs ADD COLUMN browser_checkout_pr_id INTEGER;
