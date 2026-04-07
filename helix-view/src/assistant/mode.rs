use std::fmt;
use std::sync::Arc;

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: Id,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selected {
    Current(Id),
    Pending { current: Id, next: Id },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    items: Vec<Item>,
    selected: Selected,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("mode not found")]
pub struct Missing;

impl Set {
    pub fn new(items: Vec<Item>, selected: Selected) -> Result<Self, Missing> {
        let contains = |id: &Id| items.iter().any(|item| item.id == *id);
        match &selected {
            Selected::Current(id) if contains(id) => Ok(Self { items, selected }),
            Selected::Pending { current, next } if contains(current) && contains(next) => {
                Ok(Self { items, selected })
            }
            _ => Err(Missing),
        }
    }

    #[must_use]
    pub fn selected(&self) -> &Selected {
        &self.selected
    }

    pub fn item(&self, id: &Id) -> Option<&Item> {
        self.items.iter().find(|item| item.id == *id)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter()
    }

    pub fn set_pending(&mut self, next: Id) -> Result<(), Missing> {
        if !self.items.iter().any(|item| item.id == next) {
            return Err(Missing);
        }
        self.selected = match &self.selected {
            Selected::Current(current) => Selected::Pending {
                current: current.clone(),
                next,
            },
            Selected::Pending { current, .. } => Selected::Pending {
                current: current.clone(),
                next,
            },
        };
        Ok(())
    }
}
