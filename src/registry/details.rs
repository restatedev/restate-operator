//! The live details of a deployment (docs, metadata, JSON schemas): what the per-scope read API
//! shows and the index doesn't store, fetched from the deployment's `/discover` and kept in a
//! bounded LRU keyed by deployment id. A deployment is immutable, so an entry never goes stale; it
//! is only evicted. A failed fetch (e.g. an old version scaled down) is remembered for a short
//! while, so a listing doesn't wait for the timeout on every request.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::debug;

use super::api::HttpVersion;
use super::index::{DeploymentKey, DeploymentView};
use super::manifest::{self, DISCOVER_ACCEPT, ManifestDetails};

/// The default number of deployments whose details are kept.
pub const DEFAULT_CAPACITY: usize = 1000;
/// The default timeout of a fetch.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a failed fetch is remembered.
const FAILURE_TTL: Duration = Duration::from_secs(30);
/// The largest manifest read.
const MAX_MANIFEST_BYTES: usize = 16 << 20;

pub struct DetailsCache {
    lru: Mutex<Lru>,
    timeout: Duration,
    /// h2c with prior knowledge, for `http://` (what the SDKs serve by default, and what the relay
    /// speaks).
    h2c: reqwest::Client,
    /// ALPN, for `https://`.
    https: reqwest::Client,
    /// HTTP/1.1 only, for `useHttp11`.
    http1: reqwest::Client,
}

#[derive(Clone)]
enum Cached {
    Found(Arc<ManifestDetails>),
    Failed(Instant),
}

impl DetailsCache {
    pub fn new(capacity: usize, timeout: Duration) -> Self {
        let builder = || {
            reqwest::Client::builder()
                .timeout(timeout)
                .connect_timeout(timeout)
        };
        Self {
            lru: Mutex::new(Lru::new(capacity)),
            timeout,
            h2c: builder()
                .http2_prior_knowledge()
                .build()
                .expect("an h2c client builds"),
            https: builder().build().expect("an https client builds"),
            http1: builder()
                .http1_only()
                .build()
                .expect("an http/1.1 client builds"),
        }
    }

    /// The details of `deployment`, from the cache or fetched. `None` if they can't be had.
    pub async fn get(&self, deployment: &DeploymentView) -> Option<Arc<ManifestDetails>> {
        match self.lru.lock().get(deployment.id) {
            Some(Cached::Found(details)) => return Some(details),
            Some(Cached::Failed(at)) if at.elapsed() < FAILURE_TTL => return None,
            _ => {}
        }
        let result = self.fetch(deployment).await;
        let mut lru = self.lru.lock();
        match result {
            Ok(details) => {
                let details = Arc::new(details);
                lru.insert(deployment.id, Cached::Found(Arc::clone(&details)));
                Some(details)
            }
            Err(error) => {
                debug!(address = %deployment.address, %error, "can't fetch the deployment's details");
                lru.insert(deployment.id, Cached::Failed(Instant::now()));
                None
            }
        }
    }

    /// The details of several deployments, fetched concurrently.
    pub async fn get_all(
        &self,
        deployments: impl IntoIterator<Item = &DeploymentView>,
    ) -> HashMap<DeploymentKey, Arc<ManifestDetails>> {
        let mut seen = std::collections::HashSet::new();
        let fetches = deployments
            .into_iter()
            .filter(|deployment| seen.insert(deployment.id))
            .map(|deployment| async move { (deployment.id, self.get(deployment).await) });
        futures::future::join_all(fetches)
            .await
            .into_iter()
            .filter_map(|(id, details)| Some((id, details?)))
            .collect()
    }

    async fn fetch(&self, deployment: &DeploymentView) -> Result<ManifestDetails, String> {
        let url = format!("{}/discover", deployment.address.trim_end_matches('/'));
        let client = match deployment.http_version {
            HttpVersion::Http11 => &self.http1,
            HttpVersion::Http2 if url.starts_with("http://") => &self.h2c,
            HttpVersion::Http2 => &self.https,
        };
        let mut request = client
            .get(&url)
            .header(reqwest::header::ACCEPT, DISCOVER_ACCEPT)
            .timeout(self.timeout);
        for (name, value) in headers(&deployment.headers) {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
        {
            return Err("manifest too large".to_owned());
        }
        let body = response.bytes().await.map_err(|e| e.to_string())?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err("manifest too large".to_owned());
        }
        manifest::details(&body).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    pub fn cached(&self) -> usize {
        self.lru.lock().entries.len()
    }
}

fn headers(headers: &BTreeMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
}

/// A least-recently-used map: every access stamps the entry with the next tick, the lowest tick
/// goes first.
struct Lru {
    capacity: usize,
    tick: u64,
    entries: HashMap<DeploymentKey, (Cached, u64)>,
    order: std::collections::BTreeMap<u64, DeploymentKey>,
}

impl Lru {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            tick: 0,
            entries: HashMap::new(),
            order: std::collections::BTreeMap::new(),
        }
    }

    fn get(&mut self, id: DeploymentKey) -> Option<Cached> {
        self.tick += 1;
        let tick = self.tick;
        let (value, at) = self.entries.get_mut(&id)?;
        self.order.remove(at);
        *at = tick;
        self.order.insert(tick, id);
        Some(value.clone())
    }

    fn insert(&mut self, id: DeploymentKey, value: Cached) {
        if self.capacity == 0 {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        if let Some((_, at)) = self.entries.insert(id, (value, tick)) {
            self.order.remove(&at);
        }
        self.order.insert(tick, id);
        while self.entries.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Cached {
        Cached::Found(Arc::new(ManifestDetails::default()))
    }

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut lru = Lru::new(2);
        lru.insert(1, found());
        lru.insert(2, found());
        lru.get(1);
        lru.insert(3, found());

        assert!(lru.get(1).is_some());
        assert!(lru.get(2).is_none());
        assert!(lru.get(3).is_some());
        assert_eq!(lru.entries.len(), 2);
        assert_eq!(lru.order.len(), 2);
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut lru = Lru::new(0);
        lru.insert(1, found());

        assert!(lru.get(1).is_none());
    }
}
