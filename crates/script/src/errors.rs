//! Error type surfaced to host callers.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rhai parse: {0}")]
    ParseError(String),
    #[error("rhai runtime: {0}")]
    Runtime(String),
    /// The script returned a value that doesn't match the
    /// dispatch shape (e.g. `identity_resolve` returned a string
    /// instead of a map).
    #[error("response shape: {0}")]
    BadShape(String),
    #[error("script function '{0}' not defined; plugin can't handle this method")]
    MissingFunction(&'static str),
}

pub type ScriptResult<T> = Result<T, ScriptError>;
