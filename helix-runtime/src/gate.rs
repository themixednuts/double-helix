use std::collections::VecDeque;

#[derive(Debug, PartialEq, Eq)]
pub enum Push<T> {
    Buffered,
    Ready(T),
}

#[derive(Debug, Default)]
pub struct Gate<T> {
    open: bool,
    items: VecDeque<T>,
}

impl<T> Gate<T> {
    pub fn push(&mut self, item: T) -> Push<T> {
        if self.open {
            Push::Ready(item)
        } else {
            self.items.push_back(item);
            Push::Buffered
        }
    }

    pub fn open(&mut self) -> Vec<T> {
        self.open = true;
        self.items.drain(..).collect()
    }

    pub fn is_open(&self) -> bool {
        self.open
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
