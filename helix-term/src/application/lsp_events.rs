use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use futures_util::{Stream, StreamExt};
use helix_lsp::{lsp, LanguageServerId, Notification, ServerEvent};
use helix_runtime::{
    LatestAdmissionError, LatestByKeyReceiver, LatestByKeySender, Receiver, RingReceiver,
    RingSender, Sender, Work,
};

const RELIABLE_CAPACITY: usize = 512;
const STATE_CAPACITY: usize = 1024;
const LOG_CAPACITY: usize = 1024;

type DiagnosticsKey = (LanguageServerId, lsp::Url);
type ProgressKey = (LanguageServerId, lsp::ProgressToken);

#[derive(Debug)]
pub(super) struct LspEvent {
    pub server_id: LanguageServerId,
    pub event: ServerEvent,
}

#[derive(Clone, Debug)]
pub(super) struct LspEvents {
    reliable: Sender<LspEvent>,
    diagnostics: LatestByKeySender<DiagnosticsKey, lsp::PublishDiagnosticsParams>,
    progress: LatestByKeySender<ProgressKey, VecDeque<lsp::WorkDoneProgress>>,
    logs: RingSender<(LanguageServerId, lsp::LogMessageParams)>,
    dropped_logs: Arc<AtomicU64>,
    active_streams: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub(super) struct LspEventReceiver {
    reliable: Receiver<LspEvent>,
    diagnostics: LatestByKeyReceiver<DiagnosticsKey, lsp::PublishDiagnosticsParams>,
    progress: LatestByKeyReceiver<ProgressKey, VecDeque<lsp::WorkDoneProgress>>,
    logs: RingReceiver<(LanguageServerId, lsp::LogMessageParams)>,
    pending_progress: VecDeque<LspEvent>,
    reliable_open: bool,
    diagnostics_open: bool,
    progress_open: bool,
    logs_open: bool,
}

impl LspEvents {
    pub fn channel() -> (Self, LspEventReceiver) {
        let (reliable, reliable_rx) = helix_runtime::channel(RELIABLE_CAPACITY);
        let (diagnostics, diagnostics_rx) = helix_runtime::latest_by_key(STATE_CAPACITY);
        let (progress, progress_rx) = helix_runtime::latest_by_key(STATE_CAPACITY);
        let (logs, logs_rx) = helix_runtime::ring(LOG_CAPACITY);
        (
            Self {
                reliable,
                diagnostics,
                progress,
                logs,
                dropped_logs: Arc::default(),
                active_streams: Arc::default(),
            },
            LspEventReceiver {
                reliable: reliable_rx,
                diagnostics: diagnostics_rx,
                progress: progress_rx,
                logs: logs_rx,
                pending_progress: VecDeque::new(),
                reliable_open: true,
                diagnostics_open: true,
                progress_open: true,
                logs_open: true,
            },
        )
    }

    pub fn attach<S>(&self, work: Work, mut incoming: S)
    where
        S: Stream<Item = (LanguageServerId, ServerEvent)> + Send + Unpin + 'static,
    {
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        let events = self.clone();
        work.spawn(async move {
            while let Some((server_id, event)) = incoming.next().await {
                if !events.admit(server_id, event).await {
                    break;
                }
            }
            events.active_streams.fetch_sub(1, Ordering::Relaxed);
        })
        .detach();
    }

    pub fn active_streams(&self) -> usize {
        self.active_streams.load(Ordering::Relaxed)
    }

    async fn admit(&self, server_id: LanguageServerId, event: ServerEvent) -> bool {
        match event {
            ServerEvent::Notification(Notification::PublishDiagnostics(params)) => {
                let key = (server_id, params.uri.clone());
                match self.diagnostics.try_fold(key, params, |current, incoming| {
                    if incoming
                        .version
                        .zip(current.version)
                        .is_none_or(|(new, old)| new >= old)
                    {
                        *current = incoming;
                    }
                }) {
                    Ok(_) => true,
                    Err(LatestAdmissionError::Full(key, params)) => {
                        self.diagnostics.send(key, params).await.is_ok()
                    }
                    Err(LatestAdmissionError::Closed(_, _)) => false,
                }
            }
            ServerEvent::Notification(Notification::ProgressMessage(params)) => {
                let lsp::ProgressParams { token, value } = params;
                let lsp::ProgressParamsValue::WorkDone(work) = value;
                let key = (server_id, token);
                let mut incoming = VecDeque::with_capacity(2);
                fold_progress(&mut incoming, work);
                match self.progress.try_fold(key, incoming, |current, incoming| {
                    for work in incoming {
                        fold_progress(current, work);
                    }
                }) {
                    Ok(_) => true,
                    Err(LatestAdmissionError::Full(key, pending)) => {
                        self.progress.send(key, pending).await.is_ok()
                    }
                    Err(LatestAdmissionError::Closed(_, _)) => false,
                }
            }
            ServerEvent::Notification(Notification::LogMessage(params)) => {
                match self.logs.push((server_id, params)) {
                    Ok(Some(_)) => {
                        let dropped = self.dropped_logs.fetch_add(1, Ordering::Relaxed) + 1;
                        if dropped.is_power_of_two() {
                            log::debug!(
                                "language-server log ring overwrote old records; dropped={dropped}"
                            );
                        }
                        true
                    }
                    Ok(None) => true,
                    Err(_) => false,
                }
            }
            event => self
                .reliable
                .send(LspEvent { server_id, event })
                .await
                .is_ok(),
        }
    }
}

impl LspEventReceiver {
    pub async fn recv(&mut self) -> Option<LspEvent> {
        loop {
            if let Some(event) = self.pending_progress.pop_front() {
                return Some(event);
            }
            if !self.reliable_open
                && !self.diagnostics_open
                && !self.progress_open
                && !self.logs_open
            {
                return None;
            }

            tokio::select! {
                event = self.reliable.recv(), if self.reliable_open => match event {
                    Some(event) => return Some(event),
                    None => self.reliable_open = false,
                },
                diagnostics = self.diagnostics.recv(), if self.diagnostics_open => match diagnostics {
                    Some(((server_id, _), params)) => return Some(LspEvent {
                        server_id,
                        event: ServerEvent::Notification(Notification::PublishDiagnostics(params)),
                    }),
                    None => self.diagnostics_open = false,
                },
                progress = self.progress.recv(), if self.progress_open => match progress {
                    Some((key, pending)) => {
                        self.expand_progress(key, pending);
                    }
                    None => self.progress_open = false,
                },
                log = self.logs.recv(), if self.logs_open => match log {
                    Some((server_id, params)) => return Some(LspEvent {
                        server_id,
                        event: ServerEvent::Notification(Notification::LogMessage(params)),
                    }),
                    None => self.logs_open = false,
                },
            }
        }
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<LspEvent, helix_runtime::TryRecvError> {
        use helix_runtime::TryRecvError;
        if let Some(event) = self.pending_progress.pop_front() {
            return Ok(event);
        }
        if self.reliable_open {
            match self.reliable.try_recv() {
                Ok(event) => return Ok(event),
                Err(TryRecvError::Closed) => self.reliable_open = false,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.diagnostics_open {
            match self.diagnostics.try_recv() {
                Ok(((server_id, _), params)) => {
                    return Ok(LspEvent {
                        server_id,
                        event: ServerEvent::Notification(Notification::PublishDiagnostics(params)),
                    });
                }
                Err(TryRecvError::Closed) => self.diagnostics_open = false,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.progress_open {
            match self.progress.try_recv() {
                Ok((key, pending)) => {
                    self.expand_progress(key, pending);
                    return self.pending_progress.pop_front().ok_or(TryRecvError::Empty);
                }
                Err(TryRecvError::Closed) => self.progress_open = false,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.logs_open {
            match self.logs.try_recv() {
                Ok((server_id, params)) => {
                    return Ok(LspEvent {
                        server_id,
                        event: ServerEvent::Notification(Notification::LogMessage(params)),
                    });
                }
                Err(TryRecvError::Closed) => self.logs_open = false,
                Err(TryRecvError::Empty) => {}
            }
        }

        if !self.reliable_open && !self.diagnostics_open && !self.progress_open && !self.logs_open {
            Err(TryRecvError::Closed)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn expand_progress(
        &mut self,
        (server_id, token): ProgressKey,
        pending: VecDeque<lsp::WorkDoneProgress>,
    ) {
        self.pending_progress
            .extend(pending.into_iter().map(|work| LspEvent {
                server_id,
                event: ServerEvent::Notification(Notification::ProgressMessage(
                    lsp::ProgressParams {
                        token: token.clone(),
                        value: lsp::ProgressParamsValue::WorkDone(work),
                    },
                )),
            }));
    }
}

fn fold_progress(pending: &mut VecDeque<lsp::WorkDoneProgress>, work: lsp::WorkDoneProgress) {
    match work {
        lsp::WorkDoneProgress::Begin(_) => {
            pending.clear();
            pending.push_back(work);
        }
        lsp::WorkDoneProgress::Report(_) => match pending.back_mut() {
            Some(current @ lsp::WorkDoneProgress::Report(_)) => *current = work,
            Some(lsp::WorkDoneProgress::End(_)) => {}
            _ => pending.push_back(work),
        },
        lsp::WorkDoneProgress::End(_) => {
            pending.retain(|event| matches!(event, lsp::WorkDoneProgress::Begin(_)));
            pending.push_back(work);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::TryRecvError;

    #[tokio::test]
    async fn diagnostics_are_latest_by_server_and_uri() {
        let (events, mut receiver) = LspEvents::channel();
        let server_id = LanguageServerId::default();
        let uri = lsp::Url::parse("file:///workspace/main.rs").unwrap();
        for version in [1, 3, 2] {
            assert!(
                events
                    .admit(
                        server_id,
                        ServerEvent::Notification(Notification::PublishDiagnostics(
                            lsp::PublishDiagnosticsParams {
                                uri: uri.clone(),
                                diagnostics: Vec::new(),
                                version: Some(version),
                            },
                        )),
                    )
                    .await
            );
        }

        let event = receiver.recv().await.unwrap();
        let ServerEvent::Notification(Notification::PublishDiagnostics(params)) = event.event
        else {
            panic!("expected diagnostics");
        };
        assert_eq!(params.version, Some(3));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn progress_retains_begin_latest_report_and_end() {
        let (events, mut receiver) = LspEvents::channel();
        let server_id = LanguageServerId::default();
        let token = lsp::ProgressToken::String("index".to_owned());
        let updates = [
            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                title: "Index".to_owned(),
                cancellable: None,
                message: None,
                percentage: Some(0),
            }),
            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                cancellable: None,
                message: None,
                percentage: Some(10),
            }),
            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                cancellable: None,
                message: None,
                percentage: Some(80),
            }),
            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message: None }),
        ];
        for work in updates {
            assert!(
                events
                    .admit(
                        server_id,
                        ServerEvent::Notification(Notification::ProgressMessage(
                            lsp::ProgressParams {
                                token: token.clone(),
                                value: lsp::ProgressParamsValue::WorkDone(work),
                            },
                        )),
                    )
                    .await
            );
        }

        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        assert!(matches!(
            first.event,
            ServerEvent::Notification(Notification::ProgressMessage(lsp::ProgressParams {
                value: lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::Begin(_)),
                ..
            }))
        ));
        assert!(matches!(
            second.event,
            ServerEvent::Notification(Notification::ProgressMessage(lsp::ProgressParams {
                value: lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::End(_)),
                ..
            }))
        ));
    }
}
