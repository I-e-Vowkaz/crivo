//! Sync dos PRs abertos em que eu sou reviewer ou autor.
//!
//! Por repositório: duas buscas BBQL (reviewer e autor), validadas na Fase 0, com a lista
//! `fields=` explícita (reviewers não vem por padrão). PR que estava aberto no banco e não
//! veio mais (fechado, mergeado ou eu saí dos reviewers) fica com `state = 'GONE'`.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::client::Client;
use crate::error::AppResult;

const FIELDS: &str = "next,values.id,values.title,values.description,values.state,values.created_on,\
values.updated_on,values.links.html.href,values.author.uuid,values.author.display_name,\
values.reviewers.uuid,values.reviewers.display_name,values.participants.user.uuid,\
values.participants.user.display_name,values.participants.role,values.participants.approved,\
values.participants.state,values.source.branch.name,values.source.commit.hash,\
values.source.repository.full_name,values.destination.branch.name,values.destination.commit.hash,values.comment_count";

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default)]
    values: Vec<BbPr>,
    next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BbPr {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub state: String,
    pub created_on: String,
    pub updated_on: String,
    pub links: Links,
    pub author: BbUser,
    #[serde(default)]
    pub reviewers: Vec<BbUser>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    pub source: Endpoint,
    pub destination: Endpoint,
    #[serde(default)]
    pub comment_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Links {
    pub html: Href,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Href {
    pub href: String,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BbUser {
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Participant {
    #[serde(default)]
    pub user: BbUser,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub approved: Option<bool>,
    #[serde(default)]
    pub state: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    pub branch: Branch,
    #[serde(default)]
    pub commit: Option<Commit>,
    #[serde(default)]
    pub repository: Option<Repo>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Branch {
    pub name: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    pub hash: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Repo {
    #[serde(default)]
    pub full_name: Option<String>,
}

/// Como reviewer/participante aparece na UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reviewer {
    pub name: String,
    pub uuid: Option<String>,
    pub approved: bool,
    /// `approved`, `changes_requested` ou nulo
    pub state: Option<String>,
}


async fn search(client: &Client, ws: &str, slug: &str, q: &str) -> AppResult<Vec<BbPr>> {
    let mut out = Vec::new();
    let first = format!("/repositories/{ws}/{slug}/pullrequests");
    let mut page: Page = client.get_json(&first, &[("q", q), ("pagelen", "50"), ("fields", FIELDS)]).await?.body;
    loop {
        out.append(&mut page.values);
        let Some(next) = page.next.take() else { break };
        page = client.get_json(&next, &[]).await?.body;
    }
    Ok(out)
}

/// PRs abertos do repo em que `me` é reviewer ou autor, sem duplicados, com a categoria.
pub async fn fetch_repo(client: &Client, ws: &str, slug: &str, me: &str) -> AppResult<Vec<(BbPr, &'static str)>> {
    let as_reviewer = search(client, ws, slug, &format!(r#"state="OPEN" AND reviewers.uuid="{me}""#)).await?;
    let as_author = search(client, ws, slug, &format!(r#"state="OPEN" AND author.uuid="{me}""#)).await?;
    let mut by_id: BTreeMap<i64, (BbPr, bool, bool)> = BTreeMap::new();
    for pr in as_reviewer {
        by_id.insert(pr.id, (pr, true, false));
    }
    for pr in as_author {
        by_id.entry(pr.id).and_modify(|e| e.2 = true).or_insert((pr, false, true));
    }
    Ok(by_id
        .into_values()
        .map(|(pr, rev, aut)| {
            let cat = match (rev, aut) {
                (true, true) => "both",
                (false, true) => "authored",
                _ => "review_requested",
            };
            (pr, cat)
        })
        .collect())
}

fn reviewers_of(pr: &BbPr) -> Vec<Reviewer> {
    // participantes com papel de reviewer trazem aprovação/pedido de mudança;
    // reviewers que ainda não interagiram só aparecem em `reviewers`
    let mut out: Vec<Reviewer> = pr
        .participants
        .iter()
        .filter(|p| p.role.as_deref() == Some("REVIEWER") || p.approved == Some(true) || p.state.is_some())
        .map(|p| Reviewer {
            name: p.user.display_name.clone().unwrap_or_default(),
            uuid: p.user.uuid.clone(),
            approved: p.approved.unwrap_or(false),
            state: p.state.clone(),
        })
        .collect();
    for r in &pr.reviewers {
        if !out.iter().any(|x| x.uuid.is_some() && x.uuid == r.uuid) {
            out.push(Reviewer {
                name: r.display_name.clone().unwrap_or_default(),
                uuid: r.uuid.clone(),
                approved: false,
                state: None,
            });
        }
    }
    out
}

pub fn upsert_repo_prs(conn: &mut Connection, ws: &str, slug: &str, prs: &[(BbPr, &str)]) -> AppResult<usize> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO repositories (workspace, slug) VALUES (?1, ?2) ON CONFLICT (provider, workspace, slug) DO NOTHING",
        params![ws, slug],
    )?;
    let repo_id: i64 =
        tx.query_row("SELECT id FROM repositories WHERE workspace = ?1 AND slug = ?2", params![ws, slug], |r| r.get(0))?;

    let mut seen = Vec::new();
    for (pr, cat) in prs {
        let reviewers = reviewers_of(pr);
        let changes_requested = reviewers.iter().any(|r| r.state.as_deref() == Some("changes_requested"));
        let src_sha = pr.source.commit.as_ref().map(|c| c.hash.clone()).unwrap_or_default();
        let dst_sha = pr.destination.commit.as_ref().map(|c| c.hash.clone()).unwrap_or_default();
        let prev_sha: Option<Option<String>> = tx
            .query_row(
                "SELECT last_synced_sha FROM pull_requests WHERE repository_id = ?1 AND bb_id = ?2",
                params![repo_id, pr.id],
                |r| r.get(0),
            )
            .optional()?;
        let changed = matches!(&prev_sha, Some(Some(p)) if *p != src_sha);
        tx.execute(
            "INSERT INTO pull_requests (repository_id, bb_id, title, description, url, author_uuid, author_name,
               reviewers_json, participants_json, source_branch, dest_branch, source_sha, dest_sha, source_repo,
               state, created_on, updated_on, category, changes_requested, last_synced_sha, changed_since_sync, synced_at,
               comment_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '[]', ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?11, ?19,
               strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?20)
             ON CONFLICT (repository_id, bb_id) DO UPDATE SET
               title = excluded.title, description = excluded.description, url = excluded.url,
               author_uuid = excluded.author_uuid, author_name = excluded.author_name,
               reviewers_json = excluded.reviewers_json, source_branch = excluded.source_branch,
               dest_branch = excluded.dest_branch, source_sha = excluded.source_sha, dest_sha = excluded.dest_sha,
               source_repo = excluded.source_repo, state = excluded.state, created_on = excluded.created_on,
               updated_on = excluded.updated_on, category = excluded.category,
               changes_requested = excluded.changes_requested, last_synced_sha = excluded.last_synced_sha,
               changed_since_sync = excluded.changed_since_sync, synced_at = excluded.synced_at,
               comment_count = excluded.comment_count",
            params![
                repo_id,
                pr.id,
                pr.title,
                pr.description,
                pr.links.html.href,
                pr.author.uuid.clone().unwrap_or_default(),
                pr.author.display_name,
                serde_json::to_string(&reviewers)?,
                pr.source.branch.name,
                pr.destination.branch.name,
                src_sha,
                dst_sha,
                pr.source.repository.as_ref().and_then(|r| r.full_name.clone()),
                pr.state,
                pr.created_on,
                pr.updated_on,
                cat,
                changes_requested,
                changed,
                pr.comment_count,
            ],
        )?;
        seen.push(pr.id);
    }
    // os que estavam abertos e não vieram mais
    let open: Vec<i64> = tx
        .prepare("SELECT bb_id FROM pull_requests WHERE repository_id = ?1 AND state = 'OPEN'")?
        .query_map([repo_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in open.into_iter().filter(|id| !seen.contains(id)) {
        tx.execute(
            "UPDATE pull_requests SET state = 'GONE' WHERE repository_id = ?1 AND bb_id = ?2",
            params![repo_id, id],
        )?;
    }
    tx.commit()?;
    Ok(prs.len())
}

#[cfg(test)]
mod tests {
    use super::super::client::testserver::{ok, start_with};
    use super::*;
    use crate::db;

    fn pr_json(id: i64, author: &str, sha: &str, extra: &str) -> String {
        format!(
            r#"{{"id":{id},"title":"BID-{id}","state":"OPEN","created_on":"2026-11-01T00:00:00Z",
            "updated_on":"2026-11-02T00:00:00Z","links":{{"html":{{"href":"https://bb/{id}"}}}},
            "author":{{"uuid":"{author}","display_name":"A"}},
            "source":{{"branch":{{"name":"BID-{id}"}},"commit":{{"hash":"{sha}"}},"repository":{{"full_name":"ws/r"}}}},
            "destination":{{"branch":{{"name":"dev"}},"commit":{{"hash":"d1"}}}}{extra}}}"#
        )
    }

    #[tokio::test]
    async fn junta_reviewer_e_autor_sem_duplicar_e_segue_next() {
        let me = "{me}";
        let p1 = pr_json(1, "{outro}", "s1", r#","participants":[{"user":{"uuid":"{x}","display_name":"X"},"role":"REVIEWER","approved":false,"state":"changes_requested"}]"#);
        let p2 = pr_json(2, me, "s2", "");
        // página 1 do reviewer aponta para a página 2; depois a busca de autor
        let srv = start_with(|base| {
            vec![
                ok(&format!(r#"{{"values":[{p1}],"next":"{base}/repositories/ws/r/pullrequests?page=2"}}"#)),
                ok(&format!(r#"{{"values":[{p2}]}}"#)),
                ok(&format!(r#"{{"values":[{p2}]}}"#)),
            ]
        });
        let client = Client::with_base(&srv.base, "e@x", "tok-secreto-123456").unwrap();
        let got = fetch_repo(&client, "ws", "r", me).await.unwrap();
        let cats: Vec<(i64, &str)> = got.iter().map(|(p, c)| (p.id, *c)).collect();
        assert_eq!(cats, vec![(1, "review_requested"), (2, "both")]);
        let reqs = srv.requests.lock().unwrap();
        assert!(reqs[0].contains("q=state%3D%22OPEN%22+AND+reviewers.uuid"), "{}", reqs[0]);
        assert!(reqs[1].contains("page=2"), "{}", reqs[1]);
    }

    #[test]
    fn grava_marca_mudanca_e_some_com_quem_saiu() {
        let mut conn = db::open_in_memory().unwrap();
        let parse = |s: String| serde_json::from_str::<BbPr>(&s).unwrap();
        let a = parse(pr_json(1, "{a}", "s1", r#","participants":[{"user":{"uuid":"{x}","display_name":"X"},"role":"REVIEWER","approved":false,"state":"changes_requested"}]"#));
        let b = parse(pr_json(2, "{a}", "s2", r#","reviewers":[{"uuid":"{y}","display_name":"Y"}]"#));
        upsert_repo_prs(&mut conn, "ws", "r", &[(a.clone(), "review_requested"), (b, "authored")]).unwrap();

        let row = |conn: &Connection, id: i64| -> (String, bool, bool, String) {
            conn.query_row(
                "SELECT state, changes_requested, changed_since_sync, reviewers_json FROM pull_requests WHERE bb_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        assert_eq!(row(&conn, 1).1, true, "changes_requested");
        assert!(row(&conn, 2).3.contains("\"Y\""), "reviewer sem interação aparece");

        // 2º sync: PR 1 com commit novo, PR 2 sumiu
        let a2 = parse(pr_json(1, "{a}", "s1-novo", ""));
        upsert_repo_prs(&mut conn, "ws", "r", &[(a2, "review_requested")]).unwrap();
        let (st1, cr1, ch1, _) = row(&conn, 1);
        assert_eq!((st1.as_str(), cr1, ch1), ("OPEN", false, true));
        assert_eq!(row(&conn, 2).0, "GONE");
    }
}
