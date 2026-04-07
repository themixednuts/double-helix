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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValueId(Arc<str>);

impl ValueId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ValueId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub id: ValueId,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selected {
    Current(ValueId),
    Pending { current: ValueId, next: ValueId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: Id,
    pub name: String,
    pub category: Option<String>,
    pub selected: Selected,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config value not found")]
pub struct Missing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    #[error("config option not found")]
    Item,
    #[error("config value not found")]
    Value,
}

impl Item {
    pub fn new(
        id: Id,
        name: impl Into<String>,
        category: Option<String>,
        selected: Selected,
        values: Vec<Value>,
    ) -> Result<Self, Missing> {
        let contains = |value_id: &ValueId| values.iter().any(|value| value.id == *value_id);
        match &selected {
            Selected::Current(current) if contains(current) => Ok(Self {
                id,
                name: name.into(),
                category,
                selected,
                values,
            }),
            Selected::Pending { current, next } if contains(current) && contains(next) => {
                Ok(Self {
                    id,
                    name: name.into(),
                    category,
                    selected,
                    values,
                })
            }
            _ => Err(Missing),
        }
    }
}

impl State {
    #[must_use]
    pub fn new(items: Vec<Item>) -> Self {
        Self { items }
    }

    pub fn item(&self, id: &Id) -> Option<&Item> {
        self.items.iter().find(|item| item.id == *id)
    }

    pub fn item_mut(&mut self, id: &Id) -> Option<&mut Item> {
        self.items.iter_mut().find(|item| item.id == *id)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter()
    }

    pub fn selected_value_label(&self, category: &str) -> Option<String> {
        let item = self
            .items
            .iter()
            .find(|item| item.category.as_deref() == Some(category))?;
        let selected = match &item.selected {
            Selected::Current(id) => id,
            Selected::Pending { next, .. } => next,
        };
        item.values
            .iter()
            .find(|value| &value.id == selected)
            .map(|value| value.label.clone())
            .or_else(|| Some(selected.to_string()))
    }

    pub fn cycle(&self, category: &str) -> Option<(Id, ValueId)> {
        let item = self
            .items
            .iter()
            .find(|item| item.category.as_deref() == Some(category) && !item.values.is_empty())?;
        let current = match &item.selected {
            Selected::Current(id) => id,
            Selected::Pending { next, .. } => next,
        };
        let index = item
            .values
            .iter()
            .position(|value| &value.id == current)
            .unwrap_or(0);
        let next = item.values[(index + 1) % item.values.len()].id.clone();
        Some((item.id.clone(), next))
    }

    pub fn set_pending(&mut self, id: &Id, next: ValueId) -> Result<(), SetError> {
        let item = self.item_mut(id).ok_or(SetError::Item)?;
        if !item.values.iter().any(|value| value.id == next) {
            return Err(SetError::Value);
        }
        item.selected = match &item.selected {
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
