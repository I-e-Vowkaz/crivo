mod bitbucket;
mod branches;
mod cli;
mod commands;
mod datadir;
mod db;
mod diff;
mod error;
mod fix;
mod github;
mod proc;
mod publish;
mod rc;
mod review;
mod secrets;
mod testenv;
mod triggers;

use std::sync::Mutex;

use tauri::Manager;

use commands::AppState;

/// PID do app aberto, na pasta de dados: o `comments` do terminal não mexe na correção que o
/// app está processando.
pub(crate) const APP_PID_FILE: &str = "app.pid";

/// Pasta de dados do app (`$XDG_DATA_HOME/<identifier>`), igual na janela e no terminal.
pub(crate) fn data_dir_headless() -> std::path::PathBuf {
    datadir::data_dir()
}

/// `pr-review-orchestrator --sync`: atualiza os PRs sem abrir janela (timer do systemd).
fn run_sync_headless() -> i32 {
    let db_path = data_dir_headless().join("app.db");
    if !db_path.exists() {
        eprintln!("banco não encontrado em {}; abra o app uma vez e configure o Bitbucket", db_path.display());
        return 2;
    }
    let conn = match db::open(&db_path) {
        Ok(c) => Mutex::new(c),
        Err(e) => {
            eprintln!("erro ao abrir o banco: {e}");
            return 2;
        }
    };
    match tauri::async_runtime::block_on(commands::sync_all(&conn, &secrets::KeyringStore)) {
        Ok(r) => {
            for repo in &r.repos {
                match &repo.error {
                    None => println!("{}: {} PR(s) abertos", repo.slug, repo.open),
                    Some(e) => println!("{}: ERRO {e}", repo.slug),
                }
                match &repo.fetch_error {
                    None => println!("{}: git fetch ok", repo.slug),
                    Some(e) => println!("{}: AVISO git fetch: {e}", repo.slug),
                }
            }
            println!("como {}, {} chamadas", r.user, r.requests);
            if r.repos.iter().any(|x| x.error.is_some()) { 1 } else { 0 }
        }
        Err(e) => {
            eprintln!("sync falhou: {e}");
            1
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // `-p <branch>`: review pelo terminal, como o `claude -p` (vem antes do `--sync`, que lá
    // é opção do review)
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `comments <branch>`: a tela "Meus PRs" pelo terminal
    if args.first().map(String::as_str) == Some("comments") {
        std::process::exit(cli::comments::run(&args[1..]));
    }
    // `prs`: o painel do dia
    if args.first().map(String::as_str) == Some("prs") {
        std::process::exit(cli::prs::run(&args[1..]));
    }
    if args.iter().any(|a| matches!(a.as_str(), "-p" | "--print" | "-h" | "--help")) {
        std::process::exit(cli::run(&args));
    }
    if args.iter().any(|a| a == "--sync") {
        std::process::exit(run_sync_headless());
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            // ~/.local/share/<identifier>/ no Linux. Só o usuário lê: o banco tem metadados de PRs.
            let dir = data_dir_headless();
            std::fs::create_dir_all(&dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
            std::fs::write(dir.join(APP_PID_FILE), std::process::id().to_string())?;
            let db_path = dir.join("app.db");
            // agentes, schemas e hooks: nos recursos do pacote instalado; em dev, no checkout
            if let Ok(res) = app.path().resource_dir() {
                review::set_assets_root(res.join("claude"));
            }
            let conn = db::open(&db_path)?;
            review::mark_interrupted(&conn)?;
            fix::mark_interrupted(&conn)?;
            // worktrees do chat de sessões anteriores
            let (runs_dir, root) = (dir.join("runs"), review::repos_root(&conn)?);
            tauri::async_runtime::spawn(review::chat::cleanup_worktrees(runs_dir, root));
            // ambientes de teste do .env da Fase 0: importa uma vez, senha para o cofre
            if let Some(home) = std::env::var_os("HOME") {
                let env = std::path::Path::new(&home).join(".config/pr-review-orchestrator/.env");
                if let Err(e) = testenv::import_phase0_env(&conn, &secrets::KeyringStore, &env) {
                    eprintln!("importação do .env da Fase 0 falhou: {e}");
                }
            }
            // gatilhos que um app anterior deixou rodando: a lista é lida aqui, antes de o
            // gerenciador aceitar comandos, e só ela é limpa (bloqueante: numa thread); até o
            // fim da limpeza de cada um, o gatilho fica "parando" e Rodar é recusado
            let tdir = dir.join("triggers");
            std::fs::create_dir_all(&tdir)?;
            let own_pid = std::process::id();
            let orphans = triggers::manager::scan_orphans(&tdir, own_pid);
            let trigger_mgr =
                std::sync::Arc::new(triggers::manager::Manager::new(tdir, triggers::manager::Deadlines::default()));
            if !orphans.is_empty() {
                let ids: Vec<String> = orphans.iter().map(|o| o.id.clone()).collect();
                trigger_mgr.begin_cleanup(&ids);
                let (mgr, emit) = (std::sync::Arc::clone(&trigger_mgr), commands::trigger_emit(app.handle().clone()));
                std::thread::spawn(move || {
                    mgr.cleanup_orphans(orphans, own_pid, triggers::manager::is_app_process, Some(emit))
                });
            }
            app.manage(AppState {
                triggers: trigger_mgr,
                db: Mutex::new(conn),
                secrets: Box::new(secrets::KeyringStore),
                db_path,
                reviews: Default::default(),
                review_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(5)),
                browser_lock: tokio::sync::Mutex::new(()),
                publish_lock: tokio::sync::Mutex::new(()),
                claude_limit: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::secret_status,
            commands::secret_save,
            commands::secret_delete,
            commands::setting_get,
            commands::setting_set,
            commands::bitbucket_save_token,
            commands::bitbucket_validate,
            commands::bitbucket_account,
            commands::github_save_token,
            commands::github_use_gh,
            commands::github_login,
            commands::sync_prs,
            commands::list_prs,
            commands::review_start,
            commands::review_cancel,
            commands::review_get,
            publish::publish_findings,
            rc::send_request_changes,
            commands::test_apps_get,
            commands::test_app_save,
            commands::checkout_pr,
            commands::chat_ask,
            commands::chat_get,
            commands::chat_cancel,
            commands::diff_files,
            commands::diff_file,
            commands::claude_limit_get,
            commands::claude_limit_clear,
            commands::fix_get,
            commands::fix_get_branch,
            commands::fix_start,
            commands::fix_add_findings,
            commands::fix_refresh,
            commands::fix_analyze,
            commands::fix_cancel,
            commands::fix_discard,
            commands::fix_ask,
            commands::fix_approve,
            commands::fix_commit,
            commands::fix_push,
            commands::local_branches,
            commands::review_start_branch,
            commands::usage_get,
            commands::triggers_list,
            commands::trigger_save,
            commands::trigger_delete,
            commands::trigger_start,
            commands::trigger_stop,
            commands::trigger_restart,
            commands::trigger_log,
            commands::trigger_scripts,
            commands::node_runtimes,
            commands::trigger_preview,
        ])
        .build(tauri::generate_context!())
        .expect("erro ao iniciar o app")
        .run(|app, ev| {
            if let tauri::RunEvent::Exit = ev {
                // para os gatilhos em execução antes de largar o pid do app
                app.state::<AppState>().triggers.shutdown_all();
                let _ = std::fs::remove_file(data_dir_headless().join(APP_PID_FILE));
            }
        });
}
