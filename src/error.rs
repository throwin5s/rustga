//! Errors returned by sources and the networking helpers.

/// Anything a source can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport failure: DNS, TLS, timeout, connection reset.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The site answered, but not with a success status.
    #[error("http {status} for {url}")]
    Http { status: u16, url: String },

    /// A URL was malformed or could not be joined.
    #[error("invalid url: {0}")]
    Url(String),

    /// A header name or value was rejected.
    #[error("invalid header: {0}")]
    Header(String),

    /// The response arrived but did not look like what the source expects.
    #[error("could not parse response: {0}")]
    Parse(String),

    /// The source does not implement this capability.
    #[error("source does not support {0}")]
    Unsupported(&'static str),

    /// The requested item does not exist.
    #[error("not found: {0}")]
    NotFound(String),
}

/// Shorthand used across the crate and by extensions.
pub type Result<T> = std::result::Result<T, Error>;
