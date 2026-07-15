use std::collections::{BTreeMap, BTreeSet, HashSet};

use helix_pkg::{ops::Progress, OpEvent, Ops};
use helix_runtime::{Block, Receiver, Sender, TrySend, Work};
use helix_view::DocumentId;

use crate::runtime::{ingress::RuntimeTaskSink, RuntimeTaskEvent};

const COMMAND_BOUND: usize = 32;
const PROGRESS_KEYS_BOUND: usize = 256;

#[derive(Debug)]
enum PkgProgressLane {}

struct PkgBlockingOutput {
    outcome: PkgOperationOutcome,
    runtime_change: Option<helix_loader::RuntimeAssetsChange>,
}

#[derive(Debug)]
struct PkgCommand {
    operation: PkgOperation,
    config: helix_pkg::PkgConfig,
    origin: PkgOperationOrigin,
}

#[derive(Clone, Debug)]
pub(crate) struct PkgService {
    tx: Sender<PkgCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PkgAdmissionError {
    #[error("package operation queue is full")]
    Full,
    #[error("package service is closed")]
    Closed,
}

fn progress_name(event: &OpEvent) -> &str {
    match event {
        OpEvent::Started { name }
        | OpEvent::Progress { name, .. }
        | OpEvent::Done { name }
        | OpEvent::Failed { name, .. } => name,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgOperation {
    Install(Vec<String>),
    Update(Vec<String>),
    Remove(Vec<String>),
    Sync,
    Doctor,
    DoctorPackage(String),
    Rollback(String),
    UpdateRegistries(Vec<String>),
}

impl PkgOperation {
    fn mutates_runtime_assets(&self) -> bool {
        matches!(
            self,
            Self::Install(_) | Self::Update(_) | Self::Remove(_) | Self::Sync | Self::Rollback(_)
        )
    }

    fn label(&self) -> String {
        match self {
            Self::Install(names) => operation_label("install", names),
            Self::Update(names) => operation_label("update", names),
            Self::Remove(names) => operation_label("remove", names),
            Self::Sync => "sync".into(),
            Self::Doctor => "doctor".into(),
            Self::DoctorPackage(name) => name.clone(),
            Self::Rollback(name) => name.clone(),
            Self::UpdateRegistries(names) => operation_label("registries", names),
        }
    }
}

fn operation_label(fallback: &str, names: &[String]) -> String {
    match names {
        [] => fallback.to_owned(),
        [name] => name.clone(),
        names => format!("{fallback} {} packages", names.len()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgOperationOrigin {
    User,
    MissingLanguageServer {
        documents: BTreeSet<DocumentId>,
        server: String,
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgFailure {
    pub name: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgOperationOutcome {
    pub operation: PkgOperation,
    pub origin: PkgOperationOrigin,
    pub succeeded: BTreeSet<String>,
    pub failures: Vec<PkgFailure>,
    pub warnings: Vec<String>,
    pub runtime_generation: u64,
    pub runtime_changed: bool,
}

impl PkgOperationOutcome {
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
}

impl PkgService {
    pub(crate) fn spawn(work: Work, block: Block, task_sink: RuntimeTaskSink) -> Self {
        let (service, rx) = Self::channel(COMMAND_BOUND);
        work.clone()
            .spawn(run_service(work, block, task_sink, rx))
            .detach();
        service
    }

    fn channel(bound: usize) -> (Self, Receiver<PkgCommand>) {
        let (tx, rx) = helix_runtime::channel(bound);
        (Self { tx }, rx)
    }

    pub(crate) fn submit(
        &self,
        operation: PkgOperation,
        config: helix_pkg::PkgConfig,
        origin: PkgOperationOrigin,
    ) -> Result<(), PkgAdmissionError> {
        let command = PkgCommand {
            operation,
            config,
            origin,
        };
        self.tx.try_send(command).map_err(|error| match error {
            TrySend::Full(_) => PkgAdmissionError::Full,
            TrySend::Closed(_) => PkgAdmissionError::Closed,
        })
    }
}

async fn run_service(
    work: Work,
    block: Block,
    task_sink: RuntimeTaskSink,
    mut rx: Receiver<PkgCommand>,
) {
    while let Some(command) = rx.recv().await {
        run_command(&work, &block, &task_sink, command).await;
    }
}

async fn run_command(work: &Work, block: &Block, task_sink: &RuntimeTaskSink, command: PkgCommand) {
    let PkgCommand {
        operation,
        config,
        origin,
    } = command;
    let fallback_operation = operation.clone();
    let fallback_origin = origin.clone();
    let (progress_tx, mut progress_rx) =
        helix_runtime::latest_by_key_for::<String, OpEvent, PkgProgressLane>(PROGRESS_KEYS_BOUND);
    let progress_sink = task_sink.clone();
    let progress_forwarder = work.spawn(async move {
        while let Some((_name, event)) = progress_rx.recv().await {
            if !progress_sink.send(RuntimeTaskEvent::PkgEvent(event)).await {
                break;
            }
        }
    });
    let result = block
        .spawn(move || run_blocking(operation, config, origin, progress_tx))
        .await;
    let _ = progress_forwarder.await;

    let (outcome, runtime_change) = match result {
        Ok(Ok(output)) => (output.outcome, output.runtime_change),
        Ok(Err(error)) => {
            let (outcome, event) =
                failure_outcome(fallback_operation, fallback_origin, error.to_string());
            let _ = task_sink.send(RuntimeTaskEvent::PkgEvent(event)).await;
            (outcome, None)
        }
        Err(error) => {
            let (outcome, event) = failure_outcome(
                fallback_operation,
                fallback_origin,
                format!("package task failed: {error}"),
            );
            let _ = task_sink.send(RuntimeTaskEvent::PkgEvent(event)).await;
            (outcome, None)
        }
    };
    if let Some(change) = runtime_change {
        let _ = task_sink
            .send(RuntimeTaskEvent::RuntimeAssetsChanged(change))
            .await;
    }
    let _ = task_sink
        .send(RuntimeTaskEvent::PkgOperationFinished(outcome))
        .await;
}

fn failure_outcome(
    operation: PkgOperation,
    origin: PkgOperationOrigin,
    message: String,
) -> (PkgOperationOutcome, OpEvent) {
    let name = operation.label();
    let event = OpEvent::Failed {
        name: name.clone(),
        message: message.clone(),
    };
    let runtime_generation = helix_loader::runtime_assets()
        .map(helix_loader::RuntimeAssets::generation)
        .unwrap_or_default();
    let outcome = PkgOperationOutcome {
        operation,
        origin,
        succeeded: BTreeSet::new(),
        failures: vec![PkgFailure { name, message }],
        warnings: Vec::new(),
        runtime_generation,
        runtime_changed: false,
    };
    (outcome, event)
}

fn run_blocking(
    operation: PkgOperation,
    config: helix_pkg::PkgConfig,
    origin: PkgOperationOrigin,
    progress_tx: helix_runtime::LatestByKeySender<String, OpEvent, PkgProgressLane>,
) -> anyhow::Result<PkgBlockingOutput> {
    // Capture the live projection before mutation so publication always produces an authoritative
    // old-to-new delta, including when an operation commits partially before returning an error.
    let runtime_assets = helix_loader::runtime_assets()?;
    let ops = Ops::open_with_config(config)?;
    let mut progress_state = OperationProgress::default();
    let execution = {
        let mut progress = |event: OpEvent| {
            if progress_state.accept(&event) {
                let name = progress_name(&event).to_owned();
                if let Err(error) = progress_tx.try_send(name, event) {
                    log::warn!("package progress admission failed: {error}");
                }
            }
        };
        (|| -> anyhow::Result<()> {
            match &operation {
                PkgOperation::Install(names) => {
                    run_name_batch(names, &mut progress, |name, progress| {
                        ops.install(&[name.to_owned()], progress).map(|_| ())
                    });
                }
                PkgOperation::Update(names) => {
                    if names.is_empty() {
                        ops.update(names, &mut progress)?;
                    } else {
                        run_name_batch(names, &mut progress, |name, progress| {
                            ops.update(&[name.to_owned()], progress).map(|_| ())
                        });
                    }
                }
                PkgOperation::Remove(names) => {
                    run_name_batch(names, &mut progress, |name, progress| {
                        progress(OpEvent::Started {
                            name: name.to_owned(),
                        });
                        ops.remove(&[name.to_owned()])?;
                        progress(OpEvent::Done {
                            name: name.to_owned(),
                        });
                        Ok(())
                    });
                }
                PkgOperation::Sync => ops.sync(&mut progress)?,
                PkgOperation::Doctor => {
                    progress(OpEvent::Started {
                        name: "doctor".to_owned(),
                    });
                    let report = ops.doctor()?;
                    progress(OpEvent::Progress {
                        name: "doctor".to_owned(),
                        message: format!("{} ok, {} problems", report.ok.len(), report.bad.len()),
                        percent: None,
                    });
                    progress(OpEvent::Done {
                        name: "doctor".to_owned(),
                    });
                }
                PkgOperation::DoctorPackage(name) => {
                    progress(OpEvent::Started { name: name.clone() });
                    let report = ops.doctor()?;
                    if let Some((_, message)) = report.bad.iter().find(|(bad, _)| bad == name) {
                        progress(OpEvent::Failed {
                            name: name.clone(),
                            message: message.clone(),
                        });
                    } else {
                        progress(OpEvent::Progress {
                            name: name.clone(),
                            message: "doctor ok".to_owned(),
                            percent: Some(100),
                        });
                        progress(OpEvent::Done { name: name.clone() });
                    }
                }
                PkgOperation::Rollback(name) => {
                    progress(OpEvent::Started { name: name.clone() });
                    ops.rollback(name)?;
                    progress(OpEvent::Done { name: name.clone() });
                }
                PkgOperation::UpdateRegistries(names) => {
                    let label = if names.is_empty() {
                        "registries".to_owned()
                    } else {
                        names.join(",")
                    };
                    progress(OpEvent::Started {
                        name: label.clone(),
                    });
                    let updates = ops.update_registries(names)?;
                    progress(OpEvent::Progress {
                        name: label.clone(),
                        message: format!("{} source(s) updated", updates.len()),
                        percent: Some(100),
                    });
                    progress(OpEvent::Done { name: label });
                }
            }
            Ok(())
        })()
    };

    if let Err(error) = execution {
        let event = OpEvent::Failed {
            name: operation.label(),
            message: error.to_string(),
        };
        if progress_state.accept(&event) {
            let name = progress_name(&event).to_owned();
            if let Err(error) = progress_tx.try_send(name, event) {
                log::warn!("package terminal progress admission failed: {error}");
            }
        }
    }

    let mut warnings = Vec::new();
    let mut runtime_changed = false;
    let mut runtime_change = None;
    let mut runtime_generation = runtime_assets.generation();
    if operation.mutates_runtime_assets() {
        match runtime_assets.refresh() {
            Ok(change) => {
                runtime_generation = runtime_assets.generation();
                if let Some(change) = change {
                    runtime_changed = true;
                    runtime_change = Some(change);
                }
            }
            Err(error) => warnings.push(format!(
                "runtime activation committed but live publication failed: {error}"
            )),
        }
    }

    Ok(PkgBlockingOutput {
        outcome: PkgOperationOutcome {
            operation,
            origin,
            succeeded: progress_state.succeeded,
            failures: progress_state.failures.into_values().collect(),
            warnings,
            runtime_generation,
            runtime_changed,
        },
        runtime_change,
    })
}

#[derive(Default)]
struct OperationProgress {
    terminal: HashSet<String>,
    succeeded: BTreeSet<String>,
    failures: BTreeMap<String, PkgFailure>,
}

impl OperationProgress {
    fn accept(&mut self, event: &OpEvent) -> bool {
        match event {
            OpEvent::Done { name } => {
                if !self.terminal.insert(name.clone()) {
                    return false;
                }
                self.succeeded.insert(name.clone());
                true
            }
            OpEvent::Failed { name, message } => {
                if !self.terminal.insert(name.clone()) {
                    return false;
                }
                self.failures.insert(
                    name.clone(),
                    PkgFailure {
                        name: name.clone(),
                        message: message.clone(),
                    },
                );
                true
            }
            OpEvent::Started { name } | OpEvent::Progress { name, .. } => {
                !self.terminal.contains(name)
            }
        }
    }
}

fn run_name_batch(
    names: &[String],
    progress: &mut Progress<'_>,
    mut run: impl FnMut(&str, &mut Progress<'_>) -> helix_pkg::Result<()>,
) {
    for name in names {
        if let Err(error) = run(name, progress) {
            progress(OpEvent::Failed {
                name: name.clone(),
                message: error.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_progress_emits_one_terminal_event_per_package() {
        let mut progress = OperationProgress::default();
        let failure = OpEvent::Failed {
            name: "rust-analyzer".into(),
            message: "failed".into(),
        };

        assert!(progress.accept(&failure));
        assert!(!progress.accept(&failure));
        assert!(!progress.accept(&OpEvent::Done {
            name: "rust-analyzer".into()
        }));
        assert_eq!(progress.failures.len(), 1);
        assert!(progress.succeeded.is_empty());
    }

    #[test]
    fn operation_kinds_identify_runtime_mutations() {
        assert!(PkgOperation::Install(vec!["rust-analyzer".into()]).mutates_runtime_assets());
        assert!(PkgOperation::Remove(vec!["rust-analyzer".into()]).mutates_runtime_assets());
        assert!(!PkgOperation::Doctor.mutates_runtime_assets());
        assert!(!PkgOperation::UpdateRegistries(Vec::new()).mutates_runtime_assets());
    }

    #[test]
    fn service_admission_is_bounded_fifo_and_reports_closure() {
        let (service, mut rx) = PkgService::channel(1);
        let first = PkgOperation::Install(vec!["rust-analyzer".into()]);

        assert_eq!(
            service.submit(
                first.clone(),
                helix_pkg::PkgConfig::default(),
                PkgOperationOrigin::User,
            ),
            Ok(())
        );
        assert_eq!(
            service.submit(
                PkgOperation::Sync,
                helix_pkg::PkgConfig::default(),
                PkgOperationOrigin::User,
            ),
            Err(PkgAdmissionError::Full)
        );
        assert_eq!(rx.try_recv().unwrap().operation, first);

        drop(rx);
        assert_eq!(
            service.submit(
                PkgOperation::Doctor,
                helix_pkg::PkgConfig::default(),
                PkgOperationOrigin::User,
            ),
            Err(PkgAdmissionError::Closed)
        );
    }
}
