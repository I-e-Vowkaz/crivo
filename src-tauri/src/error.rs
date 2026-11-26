//! Erro único do backend. Vai para a UI como texto, então nunca deve carregar segredo:
//! quem monta a mensagem com dados externos passa antes pelo `secrets::Redactor`.

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("banco de dados: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("cofre de senhas do sistema: {0}")]
    Secret(String),
    #[error("{0}")]
    Invalid(String),
    #[error("arquivo: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl serde::Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
