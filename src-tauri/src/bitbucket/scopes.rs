//! Escopos do API token, lidos do header `X-Oauth-Scopes` (achado 14 da Fase 0).
//! O app só aceita token **somente leitura** com os três escopos que usa: o Crivo não escreve no
//! Bitbucket.

use serde::Serialize;

pub const REQUIRED: [&str; 3] = ["read:pullrequest", "read:repository", "read:user"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeCheck {
    /// Como vieram do Bitbucket, ex.: `read:pullrequest:bitbucket`.
    pub scopes: Vec<String>,
    /// Dos obrigatórios, os que faltam.
    pub missing: Vec<String>,
    /// Qualquer escopo que não seja de leitura (`write:`, `admin:`, `delete:`...).
    pub forbidden: Vec<String>,
}

impl ScopeCheck {
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.forbidden.is_empty()
    }
}

/// `None` se o header não veio: aí não dá para provar que o token é só leitura.
pub fn parse(header: Option<&str>) -> Option<Vec<String>> {
    let h = header?;
    let mut v: Vec<String> = h
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    v.sort();
    v.dedup();
    Some(v)
}

/// `read:pullrequest:bitbucket` → `read:pullrequest`. O sufixo é o produto Atlassian.
fn base(scope: &str) -> &str {
    scope.strip_suffix(":bitbucket").unwrap_or(scope)
}

pub fn check(scopes: &[String]) -> ScopeCheck {
    let bases: Vec<&str> = scopes.iter().map(|s| base(s)).collect();
    ScopeCheck {
        scopes: scopes.to_vec(),
        missing: REQUIRED.iter().filter(|r| !bases.contains(r)).map(|r| (*r).to_owned()).collect(),
        forbidden: scopes.iter().filter(|s| !s.starts_with("read:")).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_real_da_fase0_e_aceito() {
        let s = parse(Some("read:repository:bitbucket, read:pullrequest:bitbucket, read:user:bitbucket")).unwrap();
        let c = check(&s);
        assert!(c.ok(), "{c:?}");
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn sem_header_nao_da_para_provar_leitura() {
        assert_eq!(parse(None), None);
    }

    #[test]
    fn escopo_de_escrita_e_recusado() {
        let s = parse(Some("read:pullrequest:bitbucket, read:repository:bitbucket, read:user:bitbucket, write:pullrequest:bitbucket")).unwrap();
        let c = check(&s);
        assert!(!c.ok());
        assert_eq!(c.forbidden, vec!["write:pullrequest:bitbucket"]);
        assert!(c.missing.is_empty());
    }

    #[test]
    fn admin_e_delete_tambem_sao_recusados() {
        let c = check(&parse(Some("admin:repository:bitbucket delete:repository:bitbucket")).unwrap());
        assert_eq!(c.forbidden.len(), 2);
        assert_eq!(c.missing.len(), 3);
    }

    #[test]
    fn falta_escopo_obrigatorio() {
        let c = check(&parse(Some("read:pullrequest:bitbucket")).unwrap());
        assert_eq!(c.missing, vec!["read:repository", "read:user"]);
        assert!(c.forbidden.is_empty());
    }

    #[test]
    fn aceita_escopo_sem_sufixo() {
        assert!(check(&parse(Some("read:pullrequest,read:repository,read:user")).unwrap()).ok());
    }
}
