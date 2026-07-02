use std::sync::Arc;

use helix_view::{
    editor::EditingEngineConfig,
    engine::{EditingEngine, EditingEngineFactory},
    Editor,
};

use crate::{helix::HelixEngine, registry::CommandRegistry, vim::VimEngine};

/// Creates modal editing engines from a shared command registry.
#[derive(Clone)]
pub struct ModalEngineFactory {
    registry: Arc<CommandRegistry>,
}

impl ModalEngineFactory {
    /// Create a factory backed by an existing shared command registry.
    #[must_use]
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self { registry }
    }

    /// Create a factory backed by Helix's built-in modal command registry.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::new(Arc::new(CommandRegistry::builtins()))
    }

    /// Clone the shared command registry used by engines from this factory.
    #[must_use]
    pub fn registry(&self) -> Arc<CommandRegistry> {
        Arc::clone(&self.registry)
    }

    /// Create a concrete editing engine for the requested modal paradigm.
    #[must_use]
    pub fn create_engine(&self, config: EditingEngineConfig) -> Box<dyn EditingEngine> {
        match config {
            EditingEngineConfig::Helix => Box::new(HelixEngine::new(self.registry())),
            EditingEngineConfig::Vim => Box::new(VimEngine::new(self.registry())),
        }
    }

    /// Clone this shared factory as the frontend engine factory trait object.
    #[must_use]
    pub fn shared_factory(self: &Arc<Self>) -> Arc<dyn EditingEngineFactory> {
        self.clone()
    }

    /// Install this factory into an editor's frontend state.
    pub fn install(self: &Arc<Self>, editor: &mut Editor) {
        editor.frontend_mut().engine_factory = self.shared_factory();
    }
}

impl Default for ModalEngineFactory {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl EditingEngineFactory for ModalEngineFactory {
    fn create(&self, config: EditingEngineConfig) -> Box<dyn EditingEngine> {
        self.create_engine(config)
    }
}
