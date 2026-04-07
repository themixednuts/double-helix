#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub content: String,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Replace(Vec<Item>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Failed,
}
