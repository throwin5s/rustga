//! HTTP plumbing shared by sources.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};

use crate::error::{Error, Result};

/// Browser-like user agent; most reader sites reject a bare `reqwest` one.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";

/// Per-source HTTP client.
///
/// Owns the source's headers and its rate limit (Mihon's `rateLimit`, given in
/// milliseconds) so extensions only describe *what* to request, never how fast.
/// Requests from one source are serialised by the limiter, which is what a site
/// rate limit is supposed to mean.
pub struct HttpSource {
    client: Client,
    headers: HeaderMap,
    rate_limit: Duration,
    last_request: Mutex<Option<Instant>>,
}

impl HttpSource {
    /// Client with the shared user agent and a 30 second timeout.
    pub fn new(rate_limit_ms: u64) -> Result<Self> {
        let client = Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        Self::with_client(client, rate_limit_ms)
    }

    pub fn with_client(client: Client, rate_limit_ms: u64) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        Ok(Self {
            client,
            headers,
            rate_limit: Duration::from_millis(rate_limit_ms),
            last_request: Mutex::new(None),
        })
    }

    /// Builder-style header, e.g. a `Referer` the site requires.
    pub fn with_header(mut self, name: &str, value: &str) -> Result<Self> {
        self.set_header(name, value)?;
        Ok(self)
    }

    pub fn set_header(&mut self, name: &str, value: &str) -> Result<()> {
        let header_name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Header(name.to_owned()))?;
        let header_value =
            HeaderValue::from_str(value).map_err(|_| Error::Header(name.to_owned()))?;
        self.headers.insert(header_name, header_value);
        Ok(())
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// GET with the source's headers and rate limit applied.
    pub fn get(&self, url: &str) -> Result<Response> {
        self.throttle();
        let response = self.client.get(url).headers(self.headers.clone()).send()?;
        check_status(response, url)
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        Ok(self.get(url)?.text()?)
    }

    pub fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        Ok(self.get(url)?.json()?)
    }

    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.get(url)?.bytes()?.to_vec())
    }

    fn throttle(&self) {
        if self.rate_limit.is_zero() {
            return;
        }
        let mut last = self
            .last_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.rate_limit {
                std::thread::sleep(self.rate_limit - elapsed);
            }
        }
        *last = Some(Instant::now());
    }
}

fn check_status(response: Response, url: &str) -> Result<Response> {
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Http {
            status: status.as_u16(),
            url: url.to_owned(),
        });
    }
    Ok(response)
}

/// Join a relative path onto a base URL, keeping absolute URLs intact.
pub fn join_url(base: &str, path: &str) -> Result<String> {
    let parsed =
        reqwest::Url::parse(base).map_err(|error| Error::Url(format!("{base}: {error}")))?;
    parsed
        .join(path)
        .map(|url| url.to_string())
        .map_err(|error| Error::Url(format!("{path}: {error}")))
}
