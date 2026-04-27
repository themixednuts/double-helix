use std::collections::VecDeque;

#[derive(Debug, PartialEq, Eq)]
pub enum Push<T> {
    Buffered,
    Ready(T),
}

#[derive(Debug)]
pub enum Gate<T> {
    Closed(VecDeque<T>),
    Open,
}

impl<T> Gate<T> {
    pub fn push(&mut self, item: T) -> Push<T> {
        match self {
            Self::Open => Push::Ready(item),
            Self::Closed(items) => {
                items.push_back(item);
                Push::Buffered
            }
        }
    }

    pub fn open(&mut self) -> Vec<T> {
        let items = match std::mem::replace(self, Self::Open) {
            Self::Closed(items) => items,
            Self::Open => VecDeque::new(),
        };
        items.into_iter().collect()
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

impl<T> Default for Gate<T> {
    fn default() -> Self {
        Self::Closed(VecDeque::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_buffers_until_open() {
        let mut gate = Gate::default();
        assert_eq!(gate.push(1), Push::Buffered);
        assert_eq!(gate.push(2), Push::Buffered);
        assert_eq!(gate.open(), vec![1, 2]);
        assert!(gate.is_open());
        assert_eq!(gate.push(3), Push::Ready(3));
    }
}
