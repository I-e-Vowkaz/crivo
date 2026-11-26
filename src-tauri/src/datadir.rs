//! Pasta de dados do app: `$XDG_DATA_HOME/<identifier>` (no Linux, `~/.local/share/<identifier>`),
//! a mesma que o Tauri usa.

use std::path::{Path, PathBuf};

/// Igual ao `identifier` do `tauri.conf.json`.
pub(crate) const IDENTIFIER: &str = "com.vowkaz.crivo";

/// Pasta de dados, igual na janela e no terminal.
pub(crate) fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join(IDENTIFIER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_matches_tauri_conf() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], IDENTIFIER);
    }
}
