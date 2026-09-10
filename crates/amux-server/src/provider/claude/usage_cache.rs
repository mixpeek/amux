//! One account probe for Settings, routing and background reserve. Successful
//! readings and backoff survive deploys; historical readings never route work.
use super::UsageProbe;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
pub(super) struct UsageCache {
    credential: String,
    body: Option<Value>,
    observed_at: i64,
    retry_at: i64,
    failures: u32,
    http_status: Option<u16>,
    #[serde(skip)]
    failure: Option<UsageProbe>,
}
impl UsageCache {
    pub(super) fn load(path: &Path, credential: &str, now: i64) -> Self {
        let saved = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok());
        match saved {
            Some(mut c)
                if c.credential == credential
                    && c.observed_at <= now
                    && c.body
                        .as_ref()
                        .is_none_or(|b| !super::map_usage_response(b).is_empty()) =>
            {
                if c.failures > 0 {
                    c.failure = Some(
                        c.http_status
                            .map(UsageProbe::Http)
                            .unwrap_or(UsageProbe::Transport("request")),
                    );
                }
                c
            }
            _ => Self {
                credential: credential.into(),
                ..Self::default()
            },
        }
    }
    pub(super) fn credential_matches(&self, credential: &str) -> bool {
        self.credential == credential
    }
    pub(super) fn ready(&self, now: i64) -> bool {
        now >= self.retry_at
    }
    pub(super) async fn refresh<F, Fut>(&mut self, path: &Path, now: i64, fetch: F) -> UsageProbe
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = (UsageProbe, Option<u64>)>,
    {
        if self.ready(now) {
            let (probe, retry_after) = fetch().await;
            self.record(probe, now, retry_after);
            if let Err(error) = self.save(path) {
                tracing::warn!(target:"amux::usage_probe", provider="claude", verdict="cache_write_failed", error=%error, "usage snapshot will not survive restart");
            }
        }
        self.reading()
    }
    pub(super) fn record(&mut self, probe: UsageProbe, now: i64, retry_after: Option<u64>) {
        match probe {
            UsageProbe::Ok(body) => {
                self.body = Some(body);
                self.observed_at = now;
                self.failures = 0;
                self.failure = None;
                self.http_status = None;
                self.retry_at = now + 60;
                tracing::info!(target:"amux::usage_probe", provider="claude", verdict="fresh", observed_at=now, retry_at=self.retry_at, "subscription usage recorded");
            }
            failure => {
                self.failures = self.failures.saturating_add(1);
                self.http_status = match &failure {
                    UsageProbe::Http(code) => Some(*code),
                    _ => None,
                };
                let delay = if self.http_status == Some(429) {
                    (120u64.saturating_mul(1u64 << self.failures.min(5).saturating_sub(1)))
                        .min(1800)
                } else {
                    60
                };
                self.retry_at =
                    now.saturating_add(
                        retry_after.unwrap_or(0).max(delay).min(i64::MAX as u64) as i64
                    );
                tracing::warn!(target:"amux::usage_probe", provider="claude", verdict=if self.body.is_some(){"last_known"}else{"retry_scheduled"}, http_status=?self.http_status, failures=self.failures, observed_at=self.observed_at, retry_at=self.retry_at, "subscription usage refresh deferred");
                self.failure = Some(failure);
            }
        }
    }
    pub(super) fn reading(&self) -> UsageProbe {
        let failure = self.failure.clone().map(Box::new);
        if let Some(body) = &self.body {
            UsageProbe::Snapshot {
                body: body.clone(),
                observed_at: self.observed_at,
                retry_at: self.retry_at,
                failure,
            }
        } else {
            UsageProbe::Deferred {
                failure: failure.unwrap_or_else(|| Box::new(UsageProbe::BadShape)),
                retry_at: self.retry_at,
            }
        }
    }
    pub(super) fn save(&self, path: &Path) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("usage cache has no parent"))?;
        std::fs::create_dir_all(parent)?;
        // Same-directory atomic replacement; no credentials are stored, only a
        // SHA256 identity and the usage response. Never persist an auth header.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, self)?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}
pub(super) fn cache_path() -> PathBuf {
    crate::config::amux_home().join("cache/claude-usage.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn good() -> UsageProbe {
        UsageProbe::Ok(json!({"limits":[{"kind":"session","percent":37}]}))
    }
    #[tokio::test]
    async fn simultaneous_consumers_share_one_request_and_stale_does_not_route() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let cache = Arc::new(tokio::sync::Mutex::new(UsageCache::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let (cache, calls, path) = (cache.clone(), calls.clone(), path.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .lock()
                    .await
                    .refresh(&path, 1000, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        (good(), None)
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().exact_body().is_some());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let stale = cache
            .lock()
            .await
            .refresh(&path, 1060, || async { (UsageProbe::Http(429), None) })
            .await;
        assert!(matches!(
            stale,
            UsageProbe::Snapshot {
                failure: Some(_),
                ..
            }
        ));
        assert!(
            stale.exact_body().is_none(),
            "routing must not treat historical percentages as current"
        );
    }
    #[test]
    fn snapshot_and_backoff_survive_restart_without_crossing_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let mut c = UsageCache::load(&path, "account-a", 1000);
        c.record(good(), 1000, None);
        c.record(UsageProbe::Http(429), 1060, Some(600));
        c.save(&path).unwrap();
        let restored = UsageCache::load(&path, "account-a", 1100);
        assert!(!restored.ready(1659));
        assert!(restored.ready(1660));
        match restored.reading() {
            UsageProbe::Snapshot {
                body,
                observed_at,
                retry_at,
                failure,
            } => {
                assert_eq!(body["limits"][0]["percent"], 37);
                assert_eq!(observed_at, 1000);
                assert_eq!(retry_at, 1660);
                assert_eq!(*failure.unwrap(), UsageProbe::Http(429));
            }
            other => panic!("lost reading: {other:?}"),
        }
        let switched = UsageCache::load(&path, "account-b", 1100);
        assert!(switched.body.is_none());
        assert!(switched.ready(1100));
    }
    #[test]
    fn repeated_throttling_backs_off_and_success_recovers() {
        let mut c = UsageCache::default();
        c.record(UsageProbe::Http(429), 1000, None);
        assert_eq!(c.retry_at, 1120);
        c.record(UsageProbe::Http(429), 1120, None);
        assert_eq!(c.retry_at, 1360);
        c.record(good(), 1360, None);
        assert_eq!(c.retry_at, 1420);
        assert_eq!(c.failures, 0);
        assert!(matches!(
            c.reading(),
            UsageProbe::Snapshot { failure: None, .. }
        ));
    }
    #[test]
    fn cold_failure_never_invents_a_percentage_and_corrupt_disk_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        std::fs::write(&path, "broken").unwrap();
        let mut c = UsageCache::load(&path, "a", 1000);
        c.record(UsageProbe::Http(429), 1000, None);
        assert!(matches!(
            c.reading(),
            UsageProbe::Deferred { retry_at: 1120, .. }
        ));
    }
    #[test]
    fn last_known_reading_keeps_its_original_timestamp_after_long_outage() {
        let mut c = UsageCache::default();
        c.record(good(), 1000, None);
        c.record(UsageProbe::Http(503), 100000, None);
        assert!(matches!(
            c.reading(),
            UsageProbe::Snapshot {
                observed_at: 1000,
                failure: Some(_),
                ..
            }
        ));
    }
}
