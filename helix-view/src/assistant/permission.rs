use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use super::thread;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(Arc<str>);

impl RequestId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChoiceId(Arc<str>);

impl ChoiceId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ChoiceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChoiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Custom(Cow<'static, str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: ChoiceId,
    pub label: String,
    pub kind: Kind,
}

impl Choice {
    #[must_use]
    pub fn new(id: ChoiceId, label: impl Into<String>, kind: Kind) -> Self {
        Self {
            id,
            label: label.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    id: RequestId,
    thread: thread::Id,
    title: String,
    body: String,
    default: Option<ChoiceId>,
    choices: Vec<Choice>,
}

pub struct Builder<S> {
    id: RequestId,
    thread: thread::Id,
    title: String,
    body: String,
    default: Option<ChoiceId>,
    choices: Vec<Choice>,
    _state: std::marker::PhantomData<S>,
}

pub struct Empty;
pub struct Ready;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("default choice is not present in request")]
pub struct MissingChoice;

impl Request {
    #[must_use]
    pub fn builder(
        id: RequestId,
        thread: thread::Id,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Builder<Empty> {
        Builder {
            id,
            thread,
            title: title.into(),
            body: body.into(),
            default: None,
            choices: Vec::new(),
            _state: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }
    #[must_use]
    pub fn thread(&self) -> thread::Id {
        self.thread
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
    #[must_use]
    pub fn default(&self) -> Option<&ChoiceId> {
        self.default.as_ref()
    }
    #[must_use]
    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }
}

impl Builder<Empty> {
    #[must_use]
    pub fn choice(mut self, choice: Choice) -> Builder<Ready> {
        self.choices.push(choice);
        Builder {
            id: self.id,
            thread: self.thread,
            title: self.title,
            body: self.body,
            default: self.default,
            choices: self.choices,
            _state: std::marker::PhantomData,
        }
    }
}

impl Builder<Ready> {
    #[must_use]
    pub fn choice(mut self, choice: Choice) -> Self {
        self.choices.push(choice);
        self
    }

    pub fn default(mut self, id: ChoiceId) -> Result<Self, MissingChoice> {
        if self.choices.iter().any(|choice| choice.id == id) {
            self.default = Some(id);
            Ok(self)
        } else {
            Err(MissingChoice)
        }
    }

    #[must_use]
    pub fn build(self) -> Request {
        Request {
            id: self.id,
            thread: self.thread,
            title: self.title,
            body: self.body,
            default: self.default,
            choices: self.choices,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Choose(ChoiceId),
    Dismiss,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn thread() -> thread::Id {
        thread::Id::new(NonZeroU64::new(1).unwrap())
    }

    #[test]
    fn default_choice_must_exist() {
        let req = Request::builder(RequestId::new("r1"), thread(), "title", "body")
            .choice(Choice::new(ChoiceId::new("c1"), "allow", Kind::AllowOnce));
        assert!(req.default(ChoiceId::new("missing")).is_err());
    }
}
