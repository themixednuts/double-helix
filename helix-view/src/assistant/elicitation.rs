use super::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormState {
    request_id: String,
    focused: usize,
    values: Vec<FieldValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    Select(usize),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRequired {
    pub field: String,
}

impl FormState {
    #[must_use]
    pub fn new(request: &thread::Elicitation) -> Option<Self> {
        let thread::ElicitationMode::Form { fields, .. } = &request.mode else {
            return None;
        };
        Some(Self {
            request_id: request.id.clone(),
            focused: 0,
            values: fields.iter().map(FieldValue::from_field).collect(),
        })
    }

    #[must_use]
    pub fn sync(self, request: &thread::Elicitation) -> Option<Self> {
        if self.request_id == request.id {
            let thread::ElicitationMode::Form { fields, .. } = &request.mode else {
                return None;
            };
            let mut values = self.values;
            values.resize_with(fields.len(), || FieldValue::Text(String::new()));
            for (value, field) in values.iter_mut().zip(fields) {
                if !value.matches(field) {
                    *value = FieldValue::from_field(field);
                }
                if let FieldValue::Select(index) = value {
                    *index = (*index).min(field.options.len().saturating_sub(1));
                }
            }
            return Some(Self {
                request_id: request.id.clone(),
                focused: self.focused.min(fields.len().saturating_sub(1)),
                values,
            });
        }
        Self::new(request)
    }

    #[must_use]
    pub fn focused(&self) -> usize {
        self.focused
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    #[must_use]
    pub fn value(&self, index: usize) -> Option<&FieldValue> {
        self.values.get(index)
    }

    pub fn focus_next(&mut self) {
        if !self.values.is_empty() {
            self.focused = (self.focused + 1) % self.values.len();
        }
    }

    pub fn focus_prev(&mut self) {
        if !self.values.is_empty() {
            self.focused = (self.focused + self.values.len() - 1) % self.values.len();
        }
    }

    pub fn insert_char(&mut self, ch: char) -> bool {
        let Some(FieldValue::Text(text)) = self.values.get_mut(self.focused) else {
            return false;
        };
        text.push(ch);
        true
    }

    pub fn backspace(&mut self) -> bool {
        let Some(FieldValue::Text(text)) = self.values.get_mut(self.focused) else {
            return false;
        };
        text.pop().is_some()
    }

    pub fn activate_focused(&mut self, fields: &[thread::ElicitationField], delta: isize) -> bool {
        let Some(field) = fields.get(self.focused) else {
            return false;
        };
        let Some(value) = self.values.get_mut(self.focused) else {
            return false;
        };
        match value {
            FieldValue::Bool(value) => {
                *value = !*value;
                true
            }
            FieldValue::Select(index) if !field.options.is_empty() => {
                let len = field.options.len() as isize;
                *index = (*index as isize + delta).rem_euclid(len) as usize;
                true
            }
            _ => false,
        }
    }

    pub fn submit_values(
        &self,
        fields: &[thread::ElicitationField],
    ) -> Result<Vec<(String, thread::ElicitationValue)>, MissingRequired> {
        let mut values = Vec::with_capacity(fields.len());
        for (field, value) in fields.iter().zip(&self.values) {
            match value {
                FieldValue::Text(text) => {
                    if field.required && text.trim().is_empty() {
                        return Err(MissingRequired {
                            field: field.label.clone().unwrap_or_else(|| field.name.clone()),
                        });
                    }
                    values.push((
                        field.name.clone(),
                        thread::ElicitationValue::String(text.clone()),
                    ));
                }
                FieldValue::Select(index) => {
                    let selected = field
                        .options
                        .get(*index)
                        .map(|option| option.value.clone())
                        .unwrap_or_default();
                    if field.required && selected.is_empty() {
                        return Err(MissingRequired {
                            field: field.label.clone().unwrap_or_else(|| field.name.clone()),
                        });
                    }
                    values.push((
                        field.name.clone(),
                        thread::ElicitationValue::String(selected),
                    ));
                }
                FieldValue::Bool(value) => {
                    values.push((
                        field.name.clone(),
                        thread::ElicitationValue::Boolean(*value),
                    ));
                }
            }
        }
        Ok(values)
    }
}

impl FieldValue {
    fn from_field(field: &thread::ElicitationField) -> Self {
        match field.field_type {
            thread::ElicitationFieldType::Text | thread::ElicitationFieldType::Textarea => {
                Self::Text(String::new())
            }
            thread::ElicitationFieldType::Select => Self::Select(0),
            thread::ElicitationFieldType::Bool => Self::Bool(false),
        }
    }

    fn matches(&self, field: &thread::ElicitationField) -> bool {
        matches!(
            (self, field.field_type),
            (
                Self::Text(_),
                thread::ElicitationFieldType::Text | thread::ElicitationFieldType::Textarea
            ) | (Self::Select(_), thread::ElicitationFieldType::Select)
                | (Self::Bool(_), thread::ElicitationFieldType::Bool)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> thread::Elicitation {
        thread::Elicitation {
            id: "req".to_string(),
            status: thread::ElicitationStatus::Pending,
            mode: thread::ElicitationMode::Form {
                message: "Configure".to_string(),
                fields: vec![
                    thread::ElicitationField {
                        name: "name".to_string(),
                        field_type: thread::ElicitationFieldType::Text,
                        label: Some("Name".to_string()),
                        required: true,
                        options: Vec::new(),
                    },
                    thread::ElicitationField {
                        name: "mode".to_string(),
                        field_type: thread::ElicitationFieldType::Select,
                        label: None,
                        required: true,
                        options: vec![
                            thread::ElicitationOption {
                                value: "fast".to_string(),
                                label: "Fast".to_string(),
                            },
                            thread::ElicitationOption {
                                value: "slow".to_string(),
                                label: "Slow".to_string(),
                            },
                        ],
                    },
                    thread::ElicitationField {
                        name: "confirm".to_string(),
                        field_type: thread::ElicitationFieldType::Bool,
                        label: None,
                        required: false,
                        options: Vec::new(),
                    },
                ],
            },
        }
    }

    #[test]
    fn cycles_fields_and_edits_values() {
        let request = form();
        let fields = match &request.mode {
            thread::ElicitationMode::Form { fields, .. } => fields,
            _ => unreachable!(),
        };
        let mut state = FormState::new(&request).unwrap();

        assert!(state.insert_char('a'));
        state.focus_next();
        assert!(state.activate_focused(fields, 1));
        state.focus_next();
        assert!(state.activate_focused(fields, 1));

        let values = state.submit_values(fields).unwrap();
        assert_eq!(
            values,
            vec![
                (
                    "name".to_string(),
                    thread::ElicitationValue::String("a".to_string())
                ),
                (
                    "mode".to_string(),
                    thread::ElicitationValue::String("slow".to_string())
                ),
                (
                    "confirm".to_string(),
                    thread::ElicitationValue::Boolean(true)
                ),
            ]
        );
    }

    #[test]
    fn reports_first_missing_required_field() {
        let request = form();
        let fields = match &request.mode {
            thread::ElicitationMode::Form { fields, .. } => fields,
            _ => unreachable!(),
        };
        let state = FormState::new(&request).unwrap();

        assert_eq!(
            state.submit_values(fields).unwrap_err(),
            MissingRequired {
                field: "Name".to_string()
            }
        );
    }
}
