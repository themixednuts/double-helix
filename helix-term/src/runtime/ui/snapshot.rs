use std::{fmt, marker::PhantomData, time::Instant};

use crate::runtime::{RuntimeIngress, UiCommand};

pub(crate) struct Missing;

pub(crate) struct LoadSet<F>(F);

pub(crate) struct ApplySet<F>(F);

/// Typed background snapshot request for UI state that is expensive or blocking to collect.
///
/// The builder is intentionally typestated: `spawn` is only available after a
/// blocking loader and a UI command applier have both been provided.
pub(crate) struct UiSnapshotRequest<K, O = (), Load = Missing, Apply = Missing> {
    label: &'static str,
    key: K,
    load: Load,
    apply: Apply,
    _output: PhantomData<fn() -> O>,
}

impl<K> UiSnapshotRequest<K, (), Missing, Missing> {
    pub(crate) fn new(label: &'static str, key: K) -> Self {
        Self {
            label,
            key,
            load: Missing,
            apply: Missing,
            _output: PhantomData,
        }
    }
}

impl<K, O, Apply> UiSnapshotRequest<K, O, Missing, Apply> {
    #[must_use]
    pub(crate) fn load_with<NextOutput, NextLoad>(
        self,
        load: NextLoad,
    ) -> UiSnapshotRequest<K, NextOutput, LoadSet<NextLoad>, Apply>
    where
        NextLoad: FnOnce(K) -> anyhow::Result<NextOutput> + Send + 'static,
        NextOutput: Send + 'static,
    {
        UiSnapshotRequest {
            label: self.label,
            key: self.key,
            load: LoadSet(load),
            apply: self.apply,
            _output: PhantomData,
        }
    }
}

impl<K, O, Load> UiSnapshotRequest<K, O, Load, Missing> {
    #[must_use]
    pub(crate) fn apply_with<NextApply>(
        self,
        apply: NextApply,
    ) -> UiSnapshotRequest<K, O, Load, ApplySet<NextApply>>
    where
        NextApply: FnOnce(K, O) -> UiCommand + Send + 'static,
    {
        UiSnapshotRequest {
            label: self.label,
            key: self.key,
            load: self.load,
            apply: ApplySet(apply),
            _output: PhantomData,
        }
    }
}

impl<K, O, Load, Apply> UiSnapshotRequest<K, O, LoadSet<Load>, ApplySet<Apply>>
where
    K: Clone + fmt::Debug + Send + 'static,
    O: Send + 'static,
    Load: FnOnce(K) -> anyhow::Result<O> + Send + 'static,
    Apply: FnOnce(K, O) -> UiCommand + Send + 'static,
{
    pub(crate) fn spawn(self, work: helix_runtime::Work, ingress: RuntimeIngress) {
        let label = self.label;
        let load_key = self.key.clone();
        let apply_key = self.key;
        let load = self.load.0;
        let apply = self.apply.0;

        work.spawn(async move {
            let start = Instant::now();
            log::info!("{label} phase=load_start key={load_key:?}");
            match tokio::task::spawn_blocking(move || load(load_key)).await {
                Ok(Ok(output)) => {
                    log::info!(
                        "{label} phase=load_done key={apply_key:?} elapsed_us={}",
                        start.elapsed().as_micros()
                    );
                    ingress.ui(apply(apply_key, output));
                }
                Ok(Err(err)) => {
                    log::warn!(
                        "{label} phase=load_error key={apply_key:?} error={err:#} elapsed_us={}",
                        start.elapsed().as_micros()
                    );
                }
                Err(err) => {
                    log::warn!(
                        "{label} phase=load_join_error key={apply_key:?} error={err:#} elapsed_us={}",
                        start.elapsed().as_micros()
                    );
                }
            }
        })
        .detach();
    }
}
