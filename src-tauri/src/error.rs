use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("pool: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("image: {0}")]
    Image(#[from] image::ImageError),

    #[error("join: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("migration {version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },

    #[error("config: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

pub type AppResult<T> = Result<T, AppError>;
