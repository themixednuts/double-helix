#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub id: String,
    pub name: String,
    pub terminal: Option<Terminal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Required {
        methods: Vec<Method>,
        pending_prompt: Option<String>,
        error: Option<String>,
    },
    Authenticating {
        method: Method,
    },
    Succeeded,
    Failed {
        methods: Vec<Method>,
        error: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Unknown,
    Required {
        methods: Vec<Method>,
        pending_prompt: Option<String>,
        error: Option<String>,
    },
    Authenticating {
        method: Method,
        pending_prompt: Option<String>,
    },
    Ok {
        retry_prompt: Option<String>,
    },
    Failed {
        methods: Vec<Method>,
        pending_prompt: Option<String>,
        error: String,
    },
}

impl State {
    #[must_use]
    pub fn initialized(methods: Vec<Method>) -> Self {
        if methods.is_empty() {
            Self::Ok { retry_prompt: None }
        } else {
            Self::Required {
                methods,
                pending_prompt: None,
                error: None,
            }
        }
    }

    pub fn require(&mut self, methods: Vec<Method>, pending_prompt: Option<String>) {
        *self = Self::Required {
            methods,
            pending_prompt,
            error: None,
        };
    }

    pub fn authenticate(&mut self, method_id: &str) -> bool {
        let (methods, pending_prompt) = match self {
            Self::Required {
                methods,
                pending_prompt,
                ..
            }
            | Self::Failed {
                methods,
                pending_prompt,
                ..
            } => (methods.clone(), pending_prompt.clone()),
            _ => return false,
        };
        let Some(method) = methods
            .iter()
            .find(|method| method.id == method_id)
            .cloned()
        else {
            return false;
        };
        *self = Self::Authenticating {
            method,
            pending_prompt,
        };
        true
    }

    pub fn succeed(&mut self) -> Option<String> {
        let retry_prompt = match self {
            Self::Authenticating { pending_prompt, .. } => pending_prompt.take(),
            _ => None,
        };
        *self = Self::Ok {
            retry_prompt: retry_prompt.clone(),
        };
        retry_prompt
    }

    pub fn fail(&mut self, methods: Vec<Method>, error: String) {
        let pending_prompt = match self {
            Self::Authenticating { pending_prompt, .. } => pending_prompt.take(),
            Self::Required { pending_prompt, .. } | Self::Failed { pending_prompt, .. } => {
                pending_prompt.take()
            }
            _ => None,
        };
        *self = Self::Failed {
            methods,
            pending_prompt,
            error,
        };
    }

    pub fn apply(&mut self, event: Event) -> Option<String> {
        match event {
            Event::Required {
                methods,
                pending_prompt,
                error,
            } => {
                *self = Self::Required {
                    methods,
                    pending_prompt,
                    error,
                };
                None
            }
            Event::Authenticating { method } => {
                let pending_prompt = match self {
                    Self::Required { pending_prompt, .. } | Self::Failed { pending_prompt, .. } => {
                        pending_prompt.clone()
                    }
                    _ => None,
                };
                *self = Self::Authenticating {
                    method,
                    pending_prompt,
                };
                None
            }
            Event::Succeeded => self.succeed(),
            Event::Failed { methods, error } => {
                self.fail(methods, error);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method() -> Method {
        Method {
            id: "browser".to_string(),
            name: "Browser".to_string(),
            terminal: None,
        }
    }

    #[test]
    fn transitions_from_required_to_authenticating_to_ok_and_retries_prompt() {
        let mut state = State::initialized(vec![method()]);
        state.require(vec![method()], Some("hello".to_string()));

        assert!(state.authenticate("browser"));
        assert_eq!(state.succeed(), Some("hello".to_string()));
        assert_eq!(
            state,
            State::Ok {
                retry_prompt: Some("hello".to_string())
            }
        );
    }

    #[test]
    fn failed_auth_keeps_methods_and_pending_prompt_for_retry() {
        let mut state = State::initialized(vec![method()]);
        state.require(vec![method()], Some("hello".to_string()));

        assert!(state.authenticate("browser"));
        state.fail(vec![method()], "denied".to_string());

        assert!(matches!(
            state,
            State::Failed {
                pending_prompt: Some(_),
                error,
                ..
            } if error == "denied"
        ));
    }
}
