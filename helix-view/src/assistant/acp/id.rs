use std::fmt;
use std::sync::Arc;

use super::super::backend;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Session(Arc<str>);

impl Session {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Session {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl From<Session> for backend::Remote {
    fn from(value: Session) -> Self {
        backend::Remote::new(value.0)
    }
}

impl From<&Session> for backend::Remote {
    fn from(value: &Session) -> Self {
        backend::Remote::new(value.0.clone())
    }
}
