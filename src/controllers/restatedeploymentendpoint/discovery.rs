//! Calling a deployment's `/discover`, the way Restate's admin does at registration.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use url::Url;

use crate::registry::manifest::DISCOVER_ACCEPT;

/// How long one discovery may take, connection included.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// What a deployment answered on `/discover`.
#[derive(Debug)]
pub struct Discovered {
    /// The endpoint manifest, as the SDK sent it.
    pub manifest: Vec<u8>,
    /// The `x-restate-server` response header: the SDK and its version.
    pub sdk_version: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("bad address '{0}': {1}")]
    BadAddress(String, String),
    #[error("bad header '{0}': {1}")]
    BadHeader(String, String),
    #[error("discovery request to {url} failed: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} answered {status}: {body}")]
    Status {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },
}

/// The HTTP clients discovery needs: Restate speaks HTTP/2 with prior knowledge to `http://`
/// deployments (h2c), negotiates it over TLS for `https://`, and HTTP/1.1 when asked to.
#[derive(Clone)]
pub struct DiscoveryClient {
    h2c: reqwest::Client,
    tls: reqwest::Client,
    http1: reqwest::Client,
}

impl DiscoveryClient {
    pub fn new() -> Self {
        let builder = || reqwest::Client::builder().timeout(DISCOVERY_TIMEOUT);
        Self {
            h2c: builder()
                .http2_prior_knowledge()
                .build()
                .expect("the h2c discovery client builds"),
            tls: builder().build().expect("the discovery client builds"),
            http1: builder()
                .http1_only()
                .build()
                .expect("the HTTP/1.1 discovery client builds"),
        }
    }

    pub async fn discover(
        &self,
        address: &str,
        additional_headers: &BTreeMap<String, String>,
        use_http11: bool,
    ) -> Result<Discovered, DiscoveryError> {
        let url = discover_url(address)?;

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(DISCOVER_ACCEPT));
        for (name, value) in additional_headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| DiscoveryError::BadHeader(name.clone(), e.to_string()))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| DiscoveryError::BadHeader(name.clone(), e.to_string()))?;
            headers.insert(header_name, header_value);
        }

        let client = if use_http11 {
            &self.http1
        } else if url.scheme() == "http" {
            &self.h2c
        } else {
            &self.tls
        };

        let request_error = |source| DiscoveryError::Request {
            url: url.to_string(),
            source,
        };
        let response = client
            .get(url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(request_error)?;

        let status = response.status();
        let sdk_version = response
            .headers()
            .get("x-restate-server")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DiscoveryError::Status {
                url: url.to_string(),
                status,
                body: truncate(body, 512),
            });
        }
        let manifest = response.bytes().await.map_err(request_error)?.to_vec();

        Ok(Discovered {
            manifest,
            sdk_version,
        })
    }
}

impl Default for DiscoveryClient {
    fn default() -> Self {
        Self::new()
    }
}

/// `<address>discover`, with the address taken as a directory whether or not it ends in `/`.
pub fn discover_url(address: &str) -> Result<Url, DiscoveryError> {
    let bad = |e: String| DiscoveryError::BadAddress(address.to_owned(), e);
    let mut base = Url::parse(address).map_err(|e| bad(e.to_string()))?;
    if !matches!(base.scheme(), "http" | "https") || base.host().is_none() {
        return Err(bad("must be an absolute http(s) URL".into()));
    }
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.join("discover").map_err(|e| bad(e.to_string()))
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_discover_url_keeps_the_base_path() {
        assert_eq!(
            discover_url("http://svc.ns.svc.cluster.local:9080/")
                .unwrap()
                .as_str(),
            "http://svc.ns.svc.cluster.local:9080/discover"
        );
        assert_eq!(
            discover_url("http://relay:9080/demo/sample/http/in-process/9080")
                .unwrap()
                .as_str(),
            "http://relay:9080/demo/sample/http/in-process/9080/discover"
        );
        assert_eq!(
            discover_url("https://apps.example.com/restate/")
                .unwrap()
                .as_str(),
            "https://apps.example.com/restate/discover"
        );
    }

    #[test]
    fn only_absolute_http_urls_are_addresses() {
        assert!(discover_url("ftp://x/").is_err());
        assert!(discover_url("not a url").is_err());
    }
}
