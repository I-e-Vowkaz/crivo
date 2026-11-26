//! Comentários de um PR (`/pullrequests/{id}/comments`), com o token só de leitura.
//!
//! Agrupa em threads: o comentário raiz e as respostas. Ficam de fora as threads apagadas,
//! as já resolvidas no Bitbucket, os rascunhos e as que eu mesmo abri.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::client::Client;
use crate::error::AppResult;

const FIELDS: &str = "next,values.id,values.content.raw,values.user.uuid,values.user.display_name,values.inline,\
values.parent.id,values.deleted,values.pending,values.created_on,values.resolution,values.links.html.href";

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default)]
    values: Vec<BbComment>,
    next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BbComment {
    id: i64,
    #[serde(default)]
    content: Option<Content>,
    #[serde(default)]
    user: Option<super::sync::BbUser>,
    #[serde(default)]
    inline: Option<Inline>,
    #[serde(default)]
    parent: Option<Parent>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    pending: bool,
    #[serde(default)]
    created_on: Option<String>,
    /// Presente quando a thread foi marcada como resolvida no Bitbucket.
    #[serde(default)]
    resolution: Option<serde_json::Value>,
    #[serde(default)]
    links: Option<Links>,
}

#[derive(Debug, Clone, Deserialize)]
struct Content {
    #[serde(default)]
    raw: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
struct Inline {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    #[serde(default)]
    outdated: Option<bool>,
}
#[derive(Debug, Clone, Deserialize)]
struct Parent {
    id: i64,
}
#[derive(Debug, Clone, Deserialize)]
struct Links {
    #[serde(default)]
    html: Option<super::sync::Href>,
}

pub use crate::hosting::{Reply, Thread};

fn name(c: &BbComment) -> String {
    c.user.as_ref().and_then(|u| u.display_name.clone()).unwrap_or_else(|| "?".into())
}

fn uuid(c: &BbComment) -> Option<&str> {
    c.user.as_ref().and_then(|u| u.uuid.as_deref())
}

fn body(c: &BbComment) -> String {
    c.content.as_ref().and_then(|x| x.raw.clone()).unwrap_or_default().trim().to_owned()
}

/// Threads abertas de outras pessoas, na ordem em que foram criadas. `me` é o meu uuid.
fn threads(all: Vec<BbComment>, me: &str) -> Vec<Thread> {
    let by_id: BTreeMap<i64, BbComment> = all.into_iter().map(|c| (c.id, c)).collect();
    // raiz de cada comentário (resposta de resposta sobe até a raiz)
    let root_of = |mut id: i64| {
        for _ in 0..100 {
            match by_id.get(&id).and_then(|c| c.parent.as_ref()) {
                Some(p) if by_id.contains_key(&p.id) => id = p.id,
                _ => break,
            }
        }
        id
    };
    let mut replies: BTreeMap<i64, Vec<Reply>> = BTreeMap::new();
    for c in by_id.values().filter(|c| c.parent.is_some() && !c.deleted && !c.pending) {
        replies.entry(root_of(c.id)).or_default().push(Reply { author: name(c), body: body(c), created_on: c.created_on.clone() });
    }
    by_id
        .values()
        .filter(|c| c.parent.is_none() && !c.deleted && !c.pending && c.resolution.as_ref().is_none_or(|r| r.is_null()))
        .filter(|c| uuid(c) != Some(me) && !body(c).is_empty())
        .map(|c| {
            let inline = c.inline.as_ref();
            Thread {
                id: c.id,
                author: name(c),
                body: body(c),
                path: inline.and_then(|i| i.path.clone()),
                line: inline.and_then(|i| i.to.or(i.from)),
                outdated: inline.and_then(|i| i.outdated).unwrap_or(false),
                url: c.links.as_ref().and_then(|l| l.html.as_ref()).map(|h| h.href.clone()),
                created_on: c.created_on.clone(),
                replies: replies.remove(&c.id).unwrap_or_default(),
            }
        })
        .collect()
}

pub async fn open_threads(client: &Client, ws: &str, slug: &str, bb_id: i64, me: &str) -> AppResult<Vec<Thread>> {
    let first = format!("/repositories/{ws}/{slug}/pullrequests/{bb_id}/comments");
    let mut page: Page = client.get_json(&first, &[("pagelen", "100"), ("fields", FIELDS)]).await?.body;
    let mut all = Vec::new();
    loop {
        all.append(&mut page.values);
        let Some(next) = page.next.take() else { break };
        page = client.get_json(&next, &[]).await?.body;
    }
    Ok(threads(all, me))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<BbComment> {
        serde_json::from_str::<Page>(json).unwrap().values
    }

    #[test]
    fn agrupa_respostas_e_tira_resolvidos_apagados_e_meus() {
        let all = parse(
            r#"{"values":[
              {"id":1,"content":{"raw":"use firstOrFail"},"user":{"uuid":"{ana}","display_name":"Ana"},
               "inline":{"path":"app/A.php","from":null,"to":12,"outdated":false},"created_on":"2026-11-20T10:00:00Z",
               "links":{"html":{"href":"https://bb/pr/1#comment-1"}}},
              {"id":2,"content":{"raw":"por quê?"},"user":{"uuid":"{eu}","display_name":"Eu"},"parent":{"id":1}},
              {"id":3,"content":{"raw":"porque pode vir null"},"user":{"uuid":"{ana}","display_name":"Ana"},"parent":{"id":2}},
              {"id":4,"content":{"raw":"resolvido"},"user":{"uuid":"{ana}","display_name":"Ana"},"resolution":{"type":"comment_resolution"}},
              {"id":5,"content":{"raw":"apagado"},"user":{"uuid":"{ana}","display_name":"Ana"},"deleted":true},
              {"id":6,"content":{"raw":"nota minha"},"user":{"uuid":"{eu}","display_name":"Eu"}},
              {"id":7,"content":{"raw":"rascunho"},"user":{"uuid":"{ana}","display_name":"Ana"},"pending":true},
              {"id":8,"content":{"raw":"geral"},"user":{"uuid":"{bia}","display_name":"Bia"},
               "inline":{"path":"x.ts","from":7,"to":null,"outdated":true}}
            ]}"#,
        );
        let t = threads(all, "{eu}");
        assert_eq!(t.iter().map(|x| x.id).collect::<Vec<_>>(), vec![1, 8]);
        assert_eq!(t[0].path.as_deref(), Some("app/A.php"));
        assert_eq!(t[0].line, Some(12));
        assert_eq!(t[0].replies.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(), vec!["por quê?", "porque pode vir null"]);
        assert_eq!(t[0].url.as_deref(), Some("https://bb/pr/1#comment-1"));
        // linha removida: vem do lado antigo; código mudou depois do comentário
        assert_eq!((t[1].line, t[1].outdated), (Some(7), true));
    }
}

#[cfg(test)]
mod real {
    use crate::secrets::{KeyringStore, SecretKind, SecretStore};

    /// `cargo test -- --ignored comentarios_reais --nocapture`: lê os comentários dos meus PRs
    /// abertos no Bitbucket (só leitura, token do cofre; imprime só contagens).
    #[tokio::test]
    #[ignore]
    async fn comentarios_reais() {
        let db = crate::data_dir_headless().join("app.db");
        let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (email, ws) = crate::bitbucket::account_settings(&conn).unwrap();
        let me: String = conn
            .query_row("SELECT json_extract(value_json, '$.uuid') FROM settings WHERE key = 'bitbucket.me'", [], |r| r.get(0))
            .unwrap();
        let prs: Vec<(String, i64)> = conn
            .prepare(
                "SELECT r.slug, p.bb_id FROM pull_requests p JOIN repositories r ON r.id = p.repository_id
                 WHERE p.state = 'OPEN' AND p.category != 'review_requested' ORDER BY p.updated_on DESC LIMIT 3",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let token = KeyringStore.get(&SecretKind::Bitbucket.account()).unwrap().expect("token no cofre");
        let client = crate::bitbucket::client::Client::new(&email, &token).unwrap();
        for (slug, id) in prs {
            let t = super::open_threads(&client, &ws, &slug, id, &me).await.unwrap();
            let inline = t.iter().filter(|x| x.path.is_some()).count();
            let replies: usize = t.iter().map(|x| x.replies.len()).sum();
            println!("{slug}#{id}: {} thread(s) aberta(s) de outras pessoas, {inline} em linha, {replies} resposta(s)", t.len());
            for x in &t {
                println!("   autor={} path={:?} linha={:?}", x.author, x.path, x.line);
            }
            // resposta crua, sem `fields`: quantos têm `inline`, `resolution` e `parent`
            let raw: serde_json::Value = client
                .get_json(&format!("/repositories/{ws}/{slug}/pullrequests/{id}/comments"), &[("pagelen", "100")])
                .await
                .unwrap()
                .body;
            let vals = raw["values"].as_array().cloned().unwrap_or_default();
            let has = |k: &str| vals.iter().filter(|v| !v[k].is_null()).count();
            println!("   cru: {} comentário(s), inline={}, resolution={}, parent={}, deleted={}", vals.len(), has("inline"), has("resolution"), has("parent"),
                vals.iter().filter(|v| v["deleted"] == true).count());
        }
    }
}
