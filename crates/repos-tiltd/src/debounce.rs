//! A per-key trailing debouncer for the daemon's filesystem events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

/// Coalesces bursts per key (git writes several files per operation) into a
/// single trailing call. [`stop`](Self::stop) aborts pending timers and makes
/// future schedules no-ops, so nothing fires after shutdown (which could
/// recreate a button post-teardown).
pub struct Debouncer {
    delay: Duration,
    /// `None` once stopped; otherwise the pending timer per key.
    timers: Mutex<Option<HashMap<String, JoinHandle<()>>>>,
}

impl Debouncer {
    pub fn new(delay: Duration) -> Arc<Debouncer> {
        Arc::new(Debouncer {
            delay,
            timers: Mutex::new(Some(HashMap::new())),
        })
    }

    pub fn schedule<F: FnOnce() + Send + 'static>(&self, key: String, f: F) {
        let mut guard = self.timers.lock().unwrap();
        let Some(timers) = guard.as_mut() else {
            return;
        };
        if let Some(prev) = timers.remove(&key) {
            prev.abort();
        }
        let delay = self.delay;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tokio::task::spawn_blocking(f).await;
        });
        timers.insert(key, handle);
    }

    pub fn stop(&self) {
        if let Some(timers) = self.timers.lock().unwrap().take() {
            for (_, h) in timers {
                h.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn coalesces_burst_into_single_call() {
        let d = Debouncer::new(Duration::from_millis(25));
        let calls = Arc::new(AtomicUsize::new(0));

        // A single git operation writes several files; five rapid schedules for
        // one key must collapse to one trailing call.
        for _ in 0..5 {
            let c = calls.clone();
            d.schedule("HEAD".into(), move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "burst should coalesce");
    }

    #[tokio::test]
    async fn fires_once_per_distinct_key() {
        let d = Debouncer::new(Duration::from_millis(25));
        let calls = Arc::new(AtomicUsize::new(0));

        for key in ["repo-a", "repo-b"] {
            let c = calls.clone();
            d.schedule(key.into(), move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2, "one per distinct key");
    }

    #[tokio::test]
    async fn stop_halts_pending_and_future_calls() {
        let d = Debouncer::new(Duration::from_millis(25));
        let calls = Arc::new(AtomicUsize::new(0));

        let c = calls.clone();
        d.schedule("pending".into(), move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        d.stop();
        let c = calls.clone();
        d.schedule("after-stop".into(), move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(60)).await;

        // No debounced refresh may fire once stop has run, or it could recreate
        // a button after the delete sweep.
        assert_eq!(calls.load(Ordering::SeqCst), 0, "want 0 after stop");
    }
}
