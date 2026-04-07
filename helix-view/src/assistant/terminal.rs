use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(Arc<str>);

impl Id {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Running,
    Exited { code: i32 },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub id: Id,
    pub title: Option<String>,
    pub state: State,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Open(Terminal),
    Output { id: Id, chunk: String },
    Exit { id: Id, state: State },
}
