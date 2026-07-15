use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use arc_swap::ArcSwapOption;
use helix_runtime::{LatestAdmissionError, LatestByKeySender};

use super::DocFn;

struct Request {
    generation: u64,
    query: Arc<str>,
    provider: DocFn,
}

pub(super) struct Snapshot {
    query: Arc<str>,
    documentation: Option<Arc<str>>,
}

pub(super) struct DocumentationService {
    tx: LatestByKeySender<(), Request>,
    latest_generation: Arc<AtomicU64>,
    next_generation: u64,
    requested: Option<Arc<str>>,
    snapshot: Arc<ArcSwapOption<Snapshot>>,
}

impl DocumentationService {
    pub(super) fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        redraw: helix_runtime::FrameHandle,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<(), Request>(1);
        let latest_generation = Arc::new(AtomicU64::new(0));
        let actor_generation = Arc::clone(&latest_generation);
        let snapshot = Arc::new(ArcSwapOption::empty());
        let actor_snapshot = Arc::clone(&snapshot);

        work.spawn(async move {
            while let Some(((), request)) = rx.recv().await {
                let generation = request.generation;
                let query = Arc::clone(&request.query);
                let started = std::time::Instant::now();
                let result = block
                    .spawn(move || {
                        let documentation = (request.provider)(&request.query)
                            .map(std::borrow::Cow::into_owned)
                            .map(Arc::from);
                        Snapshot {
                            query: request.query,
                            documentation,
                        }
                    })
                    .await;
                helix_view::bench::log_run_phase(
                    "prompt_documentation_actor",
                    "resolve",
                    started.elapsed(),
                    || format!("generation={generation} query_bytes={}", query.len()),
                );
                let Ok(snapshot) = result else {
                    log::error!("prompt documentation provider failed generation={generation}");
                    redraw.request_redraw();
                    continue;
                };
                if actor_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                actor_snapshot.store(Some(Arc::new(snapshot)));
                redraw.request_redraw();
            }
        })
        .detach();

        Self {
            tx,
            latest_generation,
            next_generation: 1,
            requested: None,
            snapshot,
        }
    }

    pub(super) fn resolve(&mut self, query: &str, provider: &DocFn) -> Option<Arc<str>> {
        if self.requested.as_deref() != Some(query) {
            self.submit(Arc::from(query), Arc::clone(provider));
        }
        self.snapshot
            .load_full()
            .filter(|snapshot| snapshot.query.as_ref() == query)
            .and_then(|snapshot| snapshot.documentation.as_ref().map(Arc::clone))
    }

    fn submit(&mut self, query: Arc<str>, provider: DocFn) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let previous = self.latest_generation.swap(generation, Ordering::AcqRel);
        let request = Request {
            generation,
            query: Arc::clone(&query),
            provider,
        };
        match self.tx.try_send((), request) {
            Ok(_) => self.requested = Some(query),
            Err(LatestAdmissionError::Full((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("prompt documentation admission invariant was violated");
            }
            Err(LatestAdmissionError::Closed((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("prompt documentation service is closed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex,
    };
    use std::time::Duration;

    #[test]
    fn blocked_old_provider_never_publishes_over_new_query() {
        let test = helix_runtime::test::RuntimeTest::default();
        let runtime = test.runtime();
        let mut gate = helix_runtime::FrameGate::new();
        let _redraw_rx = gate.take_receiver();
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let old_provider: DocFn = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Arc::new(move |_| {
                started.store(true, Ordering::Release);
                let (lock, wake) = &*release;
                let mut released = lock.lock().expect("release lock");
                while !*released {
                    released = wake.wait(released).expect("release wait");
                }
                Some(std::borrow::Cow::Borrowed("old"))
            })
        };
        let new_provider: DocFn = Arc::new(|_| Some(std::borrow::Cow::Borrowed("new")));
        let mut service = DocumentationService::spawn(
            runtime.work().clone(),
            runtime.block().clone(),
            gate.handle(),
        );

        test.block_on(async {
            assert!(service.resolve("old", &old_provider).is_none());
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            assert!(service.resolve("new", &new_provider).is_none());
            let (lock, wake) = &*release;
            *lock.lock().expect("release lock") = true;
            wake.notify_all();

            let documentation = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(documentation) = service.resolve("new", &new_provider) {
                        break documentation;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("new documentation should publish");
            assert_eq!(documentation.as_ref(), "new");
            assert_eq!(
                service
                    .snapshot
                    .load_full()
                    .expect("snapshot")
                    .query
                    .as_ref(),
                "new"
            );
        });
    }
}
