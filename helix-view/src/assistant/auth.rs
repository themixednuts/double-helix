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

impl Terminal {
    /// The command line, for showing the user.
    #[must_use]
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(|word| {
                if word.is_empty() || word.contains(char::is_whitespace) {
                    format!("\"{word}\"")
                } else {
                    word.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
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
    /// A login command runs in a terminal outside the editor; the user says when it finished,
    /// and only then is the agent asked to authenticate.
    TerminalLogin {
        methods: Vec<Method>,
        method: Method,
        pending_prompt: Option<String>,
        /// Why the terminal could not be opened, so the user runs the command themselves.
        error: Option<String>,
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
        *self = if method.terminal.is_some() {
            Self::TerminalLogin {
                methods,
                method,
                pending_prompt,
                error: None,
            }
        } else {
            Self::Authenticating {
                method,
                pending_prompt,
            }
        };
        true
    }

    /// The terminal login finished: authenticate with its method, whose id is returned.
    pub fn finish_terminal_login(&mut self) -> Option<String> {
        let Self::TerminalLogin {
            method,
            pending_prompt,
            ..
        } = std::mem::take(self)
        else {
            return None;
        };
        let id = method.id.clone();
        *self = Self::Authenticating {
            method,
            pending_prompt,
        };
        Some(id)
    }

    /// Back to choosing a method.
    pub fn cancel_terminal_login(&mut self) -> bool {
        let Self::TerminalLogin {
            methods,
            pending_prompt,
            ..
        } = std::mem::take(self)
        else {
            return false;
        };
        *self = Self::Required {
            methods,
            pending_prompt,
            error: None,
        };
        true
    }

    /// The terminal for the login could not be opened.
    pub fn terminal_login_failed(&mut self, message: String) {
        if let Self::TerminalLogin { error, .. } = self {
            *error = Some(message);
        }
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
            Self::Authenticating { pending_prompt, .. }
            | Self::TerminalLogin { pending_prompt, .. } => pending_prompt.take(),
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

    fn terminal_method() -> Method {
        Method {
            id: "login".to_string(),
            name: "Login".to_string(),
            terminal: Some(Terminal {
                command: "agent".to_string(),
                args: vec!["--login".to_string(), "my profile".to_string()],
                env: Vec::new(),
            }),
        }
    }

    #[test]
    fn terminal_login_waits_for_the_user_before_authenticating() {
        let mut state = State::initialized(vec![method(), terminal_method()]);
        state.require(vec![method(), terminal_method()], Some("hello".to_string()));

        assert!(state.authenticate("login"));
        assert!(matches!(state, State::TerminalLogin { .. }));
        state.terminal_login_failed("no terminal".to_string());
        assert!(matches!(
            &state,
            State::TerminalLogin { error: Some(error), .. } if error == "no terminal"
        ));

        assert_eq!(state.finish_terminal_login(), Some("login".to_string()));
        assert!(matches!(state, State::Authenticating { .. }));
        assert_eq!(state.succeed(), Some("hello".to_string()));
    }

    #[test]
    fn cancelled_terminal_login_offers_the_methods_again() {
        let mut state = State::initialized(vec![terminal_method()]);
        state.require(vec![terminal_method()], Some("hello".to_string()));
        assert!(state.authenticate("login"));

        assert!(state.cancel_terminal_login());
        assert!(matches!(
            state,
            State::Required { ref methods, pending_prompt: Some(_), .. } if methods.len() == 1
        ));
        assert_eq!(state.finish_terminal_login(), None);
    }

    #[test]
    fn terminal_command_line_quotes_words_with_spaces() {
        let method = terminal_method();
        assert_eq!(
            method.terminal.unwrap().command_line(),
            "agent --login \"my profile\""
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
