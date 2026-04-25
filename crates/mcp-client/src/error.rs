use thiserror::Error;

pub type McpResult<T> = Result<T, McpError>;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to spawn MCP server: {0}")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("server returned error: code={code} message={message}")]
    Server { code: i64, message: String },
    #[error("server closed the connection unexpectedly")]
    Closed,
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("client has been shut down")]
    ClientGone,
}
