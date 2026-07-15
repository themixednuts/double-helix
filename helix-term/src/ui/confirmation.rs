use crate::{
    compositor::{self, Context},
    ui::{self, Prompt, PromptEvent},
};

/// A deferred action gated by the standard yes/no prompt.
pub struct Confirmation {
    message: String,
    on_confirm: Option<Box<dyn FnOnce(&mut Context) + Send>>,
    on_cancel: Option<Box<dyn FnOnce(&mut Context) + Send>>,
}

impl Confirmation {
    pub fn new(
        message: impl Into<String>,
        on_confirm: impl FnOnce(&mut Context) + Send + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            on_confirm: Some(Box::new(on_confirm)),
            on_cancel: None,
        }
    }

    pub fn on_cancel(mut self, on_cancel: impl FnOnce(&mut Context) + Send + 'static) -> Self {
        self.on_cancel = Some(Box::new(on_cancel));
        self
    }

    pub(crate) fn into_prompt(self) -> Prompt {
        let Self {
            message,
            mut on_confirm,
            mut on_cancel,
        } = self;
        Prompt::new(
            format!("{message} (y/n): ").into(),
            None,
            ui::completers::none,
            move |cx: &mut Context, input: &str, event: PromptEvent| match event {
                PromptEvent::Validate if input.trim().eq_ignore_ascii_case("y") => {
                    if let Some(action) = on_confirm.take() {
                        action(cx);
                    }
                }
                PromptEvent::Validate | PromptEvent::Abort => {
                    if let Some(action) = on_cancel.take() {
                        action(cx);
                    }
                }
                PromptEvent::Update => {}
            },
        )
    }

    pub(crate) fn into_post_action(self) -> compositor::PostAction {
        compositor::PostAction::PushLayer(Box::new(self.into_prompt()))
    }
}
