use std::{collections::HashMap, time::Duration};

use helix_lsp::LanguageServerId;

#[derive(Default, Debug)]
pub struct ProgressSpinners {
    inner: HashMap<LanguageServerId, Spinner>,
}

impl ProgressSpinners {
    pub fn get(&self, id: LanguageServerId) -> Option<&Spinner> {
        self.inner.get(&id)
    }

    pub fn get_or_create(&mut self, id: LanguageServerId) -> &mut Spinner {
        self.inner.entry(id).or_default()
    }
}

#[derive(Debug)]
pub struct Spinner {
    inner: crate::widgets::Spinner,
    running: bool,
    progress: Option<u8>,
}

impl Spinner {
    pub fn dots(interval: u64) -> Self {
        Self {
            inner: crate::widgets::Spinner::dots(Duration::from_millis(interval)),
            running: false,
            progress: None,
        }
    }

    pub fn new(frames: &'static [&'static str], interval: u64) -> Self {
        Self {
            inner: crate::widgets::Spinner::new(frames, Duration::from_millis(interval)),
            running: false,
            progress: None,
        }
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::dots(80)
    }
}

impl Spinner {
    pub fn start(&mut self) {
        self.inner.restart();
        self.running = true;
    }

    pub fn frame(&self) -> Option<&str> {
        self.running.then(|| self.inner.frame())
    }

    pub fn set_progress(&mut self, progress: Option<u8>) {
        self.progress = progress.map(|progress| progress.min(100));
    }

    pub fn progress(&self) -> Option<u8> {
        self.running.then_some(self.progress).flatten()
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.progress = None;
    }

    pub fn is_stopped(&self) -> bool {
        !self.running
    }
}
