use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unknown printer: {0}")]
    UnknownPrinter(String),
    #[error("invalid report: {0}")]
    InvalidReport(String),
}
