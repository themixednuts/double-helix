use futures_util::StreamExt;
use helix_runtime::{
    LatestAdmissionError, LatestByKeyReceiver, LatestByKeySender, Receiver, Sender, Work,
};
use helix_view::assistant::{backend, thread};

const RELIABLE_CAPACITY: usize = 256;
const STREAM_CAPACITY: usize = 1024;

type StreamKey = (thread::Id, thread::StreamId);

#[derive(Clone, Debug)]
pub(super) struct AssistantEvents {
    reliable: Sender<backend::Update>,
    streams: LatestByKeySender<StreamKey, thread::NewEntry>,
}

#[derive(Debug)]
pub(super) struct AssistantEventReceiver {
    reliable: Receiver<backend::Update>,
    streams: LatestByKeyReceiver<StreamKey, thread::NewEntry>,
    reliable_open: bool,
    streams_open: bool,
}

impl AssistantEvents {
    pub fn channel() -> (Self, AssistantEventReceiver) {
        let (reliable, reliable_rx) = helix_runtime::channel(RELIABLE_CAPACITY);
        let (streams, streams_rx) = helix_runtime::latest_by_key(STREAM_CAPACITY);
        (
            Self { reliable, streams },
            AssistantEventReceiver {
                reliable: reliable_rx,
                streams: streams_rx,
                reliable_open: true,
                streams_open: true,
            },
        )
    }

    pub fn attach(&self, work: Work, mut incoming: Receiver<backend::Update>) {
        let events = self.clone();
        work.spawn(async move {
            while let Some(update) = incoming.next().await {
                match textual_stream(update) {
                    Ok((key, entry)) => {
                        match events.streams.try_fold(key, entry, merge_text_stream) {
                            Ok(_) => {}
                            Err(LatestAdmissionError::Full(key, entry)) => {
                                if events.streams.send(key, entry).await.is_err() {
                                    break;
                                }
                            }
                            Err(LatestAdmissionError::Closed(_, _)) => break,
                        }
                    }
                    Err(update) => {
                        if events.reliable.send(update).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }
}

impl AssistantEventReceiver {
    pub async fn recv(&mut self) -> Option<backend::Update> {
        loop {
            if !self.reliable_open && !self.streams_open {
                return None;
            }
            tokio::select! {
                update = self.reliable.recv(), if self.reliable_open => match update {
                    Some(update) => return Some(update),
                    None => self.reliable_open = false,
                },
                stream = self.streams.recv(), if self.streams_open => match stream {
                    Some(((thread, _), entry)) => return Some(backend::Update::Thread {
                        thread,
                        event: thread::Event::Content(thread::Content::Stream(entry)),
                    }),
                    None => self.streams_open = false,
                },
            }
        }
    }
}

fn textual_stream(
    update: backend::Update,
) -> Result<(StreamKey, thread::NewEntry), backend::Update> {
    let backend::Update::Thread { thread, event } = update else {
        return Err(update);
    };
    let thread::Event::Content(thread::Content::Stream(entry)) = event else {
        return Err(backend::Update::Thread { thread, event });
    };
    let Some(stream) = entry.stream.clone() else {
        return Err(backend::Update::Thread {
            thread,
            event: thread::Event::Content(thread::Content::Stream(entry)),
        });
    };
    if !matches!(
        &entry.kind,
        thread::EntryKind::UserPrompt { .. }
            | thread::EntryKind::AssistantText { .. }
            | thread::EntryKind::Thought { .. }
    ) {
        return Err(backend::Update::Thread {
            thread,
            event: thread::Event::Content(thread::Content::Stream(entry)),
        });
    }
    Ok(((thread, stream), entry))
}

fn merge_text_stream(current: &mut thread::NewEntry, incoming: thread::NewEntry) {
    match (&mut current.kind, incoming.kind) {
        (
            thread::EntryKind::UserPrompt { text: current },
            thread::EntryKind::UserPrompt { text: incoming },
        )
        | (
            thread::EntryKind::AssistantText { text: current },
            thread::EntryKind::AssistantText { text: incoming },
        )
        | (
            thread::EntryKind::Thought { text: current },
            thread::EntryKind::Thought { text: incoming },
        ) => current.push_str(&incoming),
        (_, kind) => current.kind = kind,
    }
    current.turn = incoming.turn;
    current.locations.extend(incoming.locations);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_chunks_fold_by_schema_stream_identity() {
        let (events, mut receiver) = AssistantEvents::channel();
        let thread = thread::Id::new(std::num::NonZeroU64::new(1).unwrap());
        let stream = thread::StreamId::assistant_text("message-1");
        for text in ["hel", "lo"] {
            let update = backend::Update::Thread {
                thread,
                event: thread::Event::Content(thread::Content::Stream(thread::NewEntry {
                    turn: None,
                    stream: Some(stream.clone()),
                    kind: thread::EntryKind::AssistantText { text: text.into() },
                    locations: Vec::new(),
                })),
            };
            let (key, entry) = textual_stream(update).unwrap();
            events
                .streams
                .try_fold(key, entry, merge_text_stream)
                .unwrap();
        }

        let backend::Update::Thread {
            event:
                thread::Event::Content(thread::Content::Stream(thread::NewEntry {
                    kind: thread::EntryKind::AssistantText { text },
                    ..
                })),
            ..
        } = receiver.recv().await.unwrap()
        else {
            panic!("expected folded assistant stream");
        };
        assert_eq!(text, "hello");
    }
}
