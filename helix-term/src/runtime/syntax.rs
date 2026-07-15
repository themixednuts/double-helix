use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use helix_runtime::{LatestAdmissionError, LatestByKeySender};
use helix_view::document::SyntaxRefreshRequest;
use helix_view::DocumentId;

use super::ingress::{RuntimeTaskEvent, RuntimeTaskSink};

#[derive(Debug, thiserror::Error)]
pub enum SyntaxAdmissionError {
    #[error("syntax service is at capacity")]
    Full,
    #[error("syntax service is closed")]
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) struct SyntaxService {
    tx: LatestByKeySender<DocumentId, SyntaxJob>,
    generations: Arc<Generations>,
    next_generation: Arc<AtomicU64>,
}

#[derive(Debug)]
struct SyntaxJob {
    generation: u64,
    request: SyntaxRefreshRequest,
}

#[derive(Debug, Default)]
struct Generations(Mutex<HashMap<DocumentId, u64>>);

impl Generations {
    fn record(&self, document: DocumentId, generation: u64) -> Option<u64> {
        self.lock().insert(document, generation)
    }

    fn is_current(&self, document: DocumentId, generation: u64) -> bool {
        self.lock().get(&document) == Some(&generation)
    }

    fn clear_if_current(&self, document: DocumentId, generation: u64) {
        let mut generations = self.lock();
        if generations.get(&document) == Some(&generation) {
            generations.remove(&document);
        }
    }

    fn restore_if_current(&self, document: DocumentId, attempted: u64, previous: Option<u64>) {
        let mut generations = self.lock();
        if generations.get(&document) != Some(&attempted) {
            return;
        }
        if let Some(previous) = previous {
            generations.insert(document, previous);
        } else {
            generations.remove(&document);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DocumentId, u64>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SyntaxService {
    pub(crate) fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        capacity: usize,
        sink: RuntimeTaskSink,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<DocumentId, SyntaxJob>(capacity);
        let generations = Arc::new(Generations::default());
        let actor_generations = generations.clone();

        work.spawn(async move {
            while let Some((document, job)) = rx.recv().await {
                let SyntaxJob {
                    generation,
                    request,
                } = job;
                let version = request.version;
                let started = std::time::Instant::now();
                let result = block.spawn(move || request.execute()).await;
                if !actor_generations.is_current(document, generation) {
                    continue;
                }

                let keep_running = match result {
                    Ok(Ok(syntax)) => {
                        sink
                            .send(RuntimeTaskEvent::ApplySyntax {
                                document,
                                version,
                                syntax,
                            })
                            .await
                    }
                    Ok(Err(error)) => {
                        log::warn!(
                            "[syntax_service] parse_failed document={document:?} version={version} elapsed_us={} error={error}",
                            started.elapsed().as_micros(),
                        );
                        true
                    }
                    Err(error) => {
                        log::warn!(
                            "[syntax_service] worker_failed document={document:?} version={version} elapsed_us={} error={error}",
                            started.elapsed().as_micros(),
                        );
                        true
                    }
                };
                actor_generations.clear_if_current(document, generation);
                if !keep_running {
                    break;
                }
            }
        })
        .detach();

        Self {
            tx,
            generations,
            next_generation: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) fn submit(&self, request: SyntaxRefreshRequest) -> Result<(), SyntaxAdmissionError> {
        let document = request.document;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let previous = self.generations.record(document, generation);
        match self.tx.try_send(
            document,
            SyntaxJob {
                generation,
                request,
            },
        ) {
            Ok(_) => Ok(()),
            Err(LatestAdmissionError::Full(_, _)) => {
                self.restore_generation(document, generation, previous);
                Err(SyntaxAdmissionError::Full)
            }
            Err(LatestAdmissionError::Closed(_, _)) => {
                self.restore_generation(document, generation, previous);
                Err(SyntaxAdmissionError::Closed)
            }
        }
    }

    fn restore_generation(&self, document: DocumentId, attempted: u64, previous: Option<u64>) {
        self.generations
            .restore_if_current(document, attempted, previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_completion_does_not_clear_newer_generation() {
        let generations = Generations::default();
        let document = DocumentId::new(std::num::NonZeroUsize::MIN);

        generations.record(document, 1);
        generations.record(document, 2);
        generations.clear_if_current(document, 1);

        assert!(generations.is_current(document, 2));
        generations.clear_if_current(document, 2);
        assert!(!generations.is_current(document, 2));
        assert!(generations.lock().is_empty());
    }

    #[test]
    fn rejected_admission_restores_previous_generation() {
        let generations = Generations::default();
        let document = DocumentId::new(std::num::NonZeroUsize::MIN);

        generations.record(document, 1);
        let previous = generations.record(document, 2);
        generations.restore_if_current(document, 2, previous);

        assert!(generations.is_current(document, 1));
    }
}
