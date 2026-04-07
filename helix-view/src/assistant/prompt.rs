use std::marker::PhantomData;

use super::{context, thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub uri: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub uri: String,
    pub mime: Option<String>,
    pub text: Option<String>,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Image(Image),
    Audio(Audio),
    Link(Link),
    Resource(Resource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    thread: thread::Id,
    role: Role,
    parts: Vec<Part>,
}

pub struct Builder<S> {
    thread: thread::Id,
    role: Role,
    parts: Vec<Part>,
    _state: PhantomData<S>,
}

pub struct Empty;
pub struct Ready;

impl Request {
    #[must_use]
    pub fn builder(thread: thread::Id, role: Role) -> Builder<Empty> {
        Builder {
            thread,
            role,
            parts: Vec::new(),
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn thread(&self) -> thread::Id {
        self.thread
    }
    #[must_use]
    pub fn role(&self) -> &Role {
        &self.role
    }
    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }
}

impl Builder<Empty> {
    #[must_use]
    pub fn text(self, text: impl Into<String>) -> Builder<Ready> {
        Builder {
            thread: self.thread,
            role: self.role,
            parts: vec![Part::Text(text.into())],
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn part(self, part: Part) -> Builder<Ready> {
        Builder {
            thread: self.thread,
            role: self.role,
            parts: vec![part],
            _state: PhantomData,
        }
    }

    #[must_use]
    pub fn context(self, item: context::Kind) -> Builder<Ready> {
        self.part(context_part(item))
    }
}

impl Builder<Ready> {
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.parts.push(Part::Text(text.into()));
        self
    }

    #[must_use]
    pub fn part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self
    }

    #[must_use]
    pub fn push_context(self, item: context::Kind) -> Self {
        self.part(context_part(item))
    }

    #[must_use]
    pub fn build(self) -> Request {
        Request {
            thread: self.thread,
            role: self.role,
            parts: self.parts,
        }
    }
}

fn context_part(item: context::Kind) -> Part {
    match item {
        context::Kind::Selection(selection) => Part::Resource(Resource {
            uri: file_uri(&selection.path),
            mime: Some("text/plain".to_string()),
            text: Some(selection.text),
            data: None,
        }),
        context::Kind::Symbol(symbol) => Part::Resource(Resource {
            uri: file_uri(&symbol.path),
            mime: Some("text/plain".to_string()),
            text: Some(symbol.text),
            data: None,
        }),
        context::Kind::File(file) => Part::Resource(Resource {
            uri: file_uri(&file.path),
            mime: None,
            text: None,
            data: None,
        }),
        context::Kind::Diagnostics(diagnostics) => Part::Resource(Resource {
            uri: file_uri(&diagnostics.path),
            mime: Some("text/plain".to_string()),
            text: Some(diagnostics.items.join("\n")),
            data: None,
        }),
        context::Kind::Diff(diff) => Part::Resource(Resource {
            uri: file_uri(&diff.path),
            mime: Some("text/plain".to_string()),
            text: Some(diff.summary),
            data: None,
        }),
    }
}

fn file_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}
