#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Revision(u64);

impl Revision {
    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn next(&mut self) -> Self {
        self.0 = self.0.wrapping_add(1);
        *self
    }
}

impl From<u64> for Revision {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
