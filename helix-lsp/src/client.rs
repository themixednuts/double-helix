use crate::{
    file_operations::FileOperationsInterest,
    jsonrpc,
    transport::{
        DocumentChangeTarget, DocumentUpdate, InitializationState, Payload, Transport,
        TransportHandle,
    },
    ClientLaunchIdentity, Error, LanguageServerId, OffsetEncoding, Result, ServerEvent,
};

use crate::lsp::{
    self, notification::DidChangeWorkspaceFolders, CodeActionCapabilityResolveSupport,
    DidChangeWorkspaceFoldersParams, OneOf, PositionEncodingKind, SignatureHelp, Url,
    WorkspaceFolder, WorkspaceFoldersChangeEvent,
};
use arc_swap::ArcSwap;
use helix_core::{syntax::config::LanguageServerFeature, ChangeSet, Rope};
use helix_loader::{ResolvedLaunch, VERSION_AND_GIT_HASH};
use helix_runtime::Receiver;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf};
use std::{
    ffi::OsStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use std::{future::Future, sync::OnceLock};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::{Mutex as AsyncMutex, OnceCell},
};

fn workspace_for_uri(uri: lsp::Url) -> WorkspaceFolder {
    lsp::WorkspaceFolder {
        name: uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .map(|basename| basename.to_string())
            .unwrap_or_default(),
        uri,
    }
}

#[derive(Debug)]
struct WorkspaceFolders {
    snapshot: ArcSwap<Vec<WorkspaceFolder>>,
}

impl WorkspaceFolders {
    fn new(folders: Vec<WorkspaceFolder>) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(folders),
        }
    }

    fn snapshot(&self) -> Arc<Vec<WorkspaceFolder>> {
        self.snapshot.load_full()
    }

    fn contains(&self, uri: &lsp::Url) -> bool {
        self.snapshot
            .load()
            .iter()
            .any(|workspace| &workspace.uri == uri)
    }

    fn insert(&self, folder: WorkspaceFolder) -> bool {
        loop {
            let current = self.snapshot.load_full();
            if current.iter().any(|workspace| workspace.uri == folder.uri) {
                return false;
            }

            let mut next = (*current).clone();
            next.push(folder.clone());
            let previous = arc_swap::Guard::into_inner(
                self.snapshot.compare_and_swap(&current, Arc::new(next)),
            );
            if Arc::ptr_eq(&previous, &current) {
                return true;
            }
        }
    }
}

fn file_operation_uri(path: &Path, is_dir: bool) -> Option<String> {
    let url = if is_dir {
        Url::from_directory_path(path)
    } else {
        Url::from_file_path(path)
    };
    Some(url.ok()?.to_string())
}

fn client_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .filter_map(|locale| normalize_locale(&locale))
        .next()
}

fn launch_arguments<'a>(
    prefix_args: &'a [String],
    default_args: &'a [String],
    configured_args: &'a [String],
) -> impl Iterator<Item = &'a str> {
    prefix_args
        .iter()
        .chain(default_args)
        .chain(configured_args)
        .map(String::as_str)
}

fn normalize_locale(locale: &str) -> Option<String> {
    let locale = locale.split(['.', '@']).next().unwrap_or(locale);
    if locale.is_empty() || matches!(locale, "C" | "POSIX") {
        return None;
    }
    Some(locale.replace('_', "-"))
}

#[derive(Debug)]
pub struct Client {
    id: LanguageServerId,
    name: String,
    launch_identity: ClientLaunchIdentity,
    process: AsyncMutex<Child>,
    transport: TransportHandle,
    request_counter: AtomicU64,
    pub(crate) capabilities: OnceCell<lsp::ServerCapabilities>,
    pub(crate) file_operation_interest: OnceLock<FileOperationsInterest>,
    config: Option<Value>,
    root_path: std::path::PathBuf,
    root_uri: Option<lsp::Url>,
    workspace_folders: WorkspaceFolders,
    /// workspace folders added while the server is still initializing
    req_timeout: u64,
}

impl Client {
    pub(crate) fn try_add_prepared_doc(
        self: &Arc<Self>,
        root_path: &Path,
        root_uri: Option<lsp::Url>,
        may_support_workspace: bool,
    ) -> bool {
        if matches!(
            self.transport.initialization_state(),
            InitializationState::Failed(_) | InitializationState::Closed
        ) {
            return false;
        }

        if self.root_path == root_path
            || root_uri
                .as_ref()
                .is_some_and(|root_uri| self.workspace_folders.contains(root_uri))
        {
            // workspace URI is already registered so we can use this client
            return true;
        }

        // this server definitely doesn't support multiple workspace, no need to check capabilities
        if !may_support_workspace {
            return false;
        }

        let Some(capabilities) = self.capabilities.get() else {
            let client = Arc::clone(self);
            // initialization hasn't finished yet, deal with this new root later
            // TODO: In the edgecase that a **new root** is added
            // for an LSP that **doesn't support workspace_folders** before initaliation is finished
            // the new roots are ignored.
            // That particular edgecase would require retroactively spawning new LSP
            // clients and therefore also require us to retroactively update the corresponding
            // documents LSP client handle. It's doable but a pretty weird edgecase so let's
            // wait and see if anyone ever runs into it.
            tokio::spawn(async move {
                if !matches!(
                    client.transport.wait_for_initialization().await,
                    InitializationState::Initialized
                ) {
                    return;
                }
                let Some(capabilities) = client.capabilities.get() else {
                    return;
                };
                if let Some(workspace_folders_caps) = capabilities
                    .workspace
                    .as_ref()
                    .and_then(|cap| cap.workspace_folders.as_ref())
                    .filter(|cap| cap.supported.unwrap_or(false))
                {
                    client.add_workspace_folder(
                        root_uri,
                        workspace_folders_caps.change_notifications.as_ref(),
                    );
                }
            });
            return true;
        };

        if let Some(workspace_folders_caps) = capabilities
            .workspace
            .as_ref()
            .and_then(|cap| cap.workspace_folders.as_ref())
            .filter(|cap| cap.supported.unwrap_or(false))
        {
            self.add_workspace_folder(
                root_uri,
                workspace_folders_caps.change_notifications.as_ref(),
            );
            true
        } else {
            // the server doesn't support multi workspaces, we need a new client
            false
        }
    }

    fn add_workspace_folder(
        &self,
        root_uri: Option<lsp::Url>,
        change_notifications: Option<&OneOf<bool, String>>,
    ) {
        // root_uri is None just means that there isn't really any LSP workspace
        // associated with this file. For servers that support multiple workspaces
        // there is just one server so we can always just use that shared instance.
        // No need to add a new workspace root here as there is no logical root for this file
        // let the server deal with this
        let Some(root_uri) = root_uri else {
            return;
        };

        // server supports workspace folders, let's add the new root to the list
        if !self
            .workspace_folders
            .insert(workspace_for_uri(root_uri.clone()))
        {
            return;
        }
        if Some(&OneOf::Left(false)) == change_notifications {
            // server specifically opted out of DidWorkspaceChange notifications
            // let's assume the server will request the workspace folders itself
            // and that we can therefore reuse the client (but are done now)
            return;
        }
        self.did_change_workspace(vec![workspace_for_uri(root_uri)], Vec::new())
    }

    /// Merge FormattingOptions with 'config.format' and return it
    fn get_merged_formatting_options(
        &self,
        options: lsp::FormattingOptions,
    ) -> lsp::FormattingOptions {
        let config_format = self
            .config
            .as_ref()
            .and_then(|cfg| cfg.get("format"))
            .and_then(|fmt| HashMap::<String, lsp::FormattingProperty>::deserialize(fmt).ok());

        if let Some(mut properties) = config_format {
            // passed in options take precedence over 'config.format'
            properties.extend(options.properties);
            lsp::FormattingOptions {
                properties,
                ..options
            }
        } else {
            options
        }
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub(crate) fn start_with_launch(
        launch: ResolvedLaunch,
        args: &[String],
        config: Option<Value>,
        server_environment: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
        root_path: PathBuf,
        root_uri: Option<lsp::Url>,
        id: LanguageServerId,
        name: String,
        launch_identity: ClientLaunchIdentity,
        req_timeout: u64,
    ) -> Result<(Self, Receiver<(LanguageServerId, ServerEvent)>)> {
        let ResolvedLaunch {
            program,
            prefix_args,
            default_args,
            env,
            ..
        } = launch;
        let process = Command::new(program)
            .envs(env)
            .envs(server_environment)
            .args(launch_arguments(&prefix_args, &default_args, args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&root_path)
            // make sure the process is reaped on drop
            .kill_on_drop(true)
            .spawn();

        let mut process = process?;

        // TODO: do we need bufreader/writer here? or do we use async wrappers on unblock?
        let writer = BufWriter::new(process.stdin.take().expect("Failed to open stdin"));
        let reader = BufReader::new(process.stdout.take().expect("Failed to open stdout"));
        let stderr = BufReader::new(process.stderr.take().expect("Failed to open stderr"));

        let (server_rx, transport) = Transport::start(reader, writer, stderr, id, name.clone());

        let workspace_folders = root_uri
            .clone()
            .map(|root| vec![workspace_for_uri(root)])
            .unwrap_or_default();

        let client = Self {
            id,
            name,
            launch_identity,
            process: AsyncMutex::new(process),
            transport,
            request_counter: AtomicU64::new(0),
            capabilities: OnceCell::new(),
            file_operation_interest: OnceLock::new(),
            config,
            req_timeout,
            root_path,
            root_uri,
            workspace_folders: WorkspaceFolders::new(workspace_folders),
        };

        Ok((client, server_rx))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> LanguageServerId {
        self.id
    }

    pub(crate) fn launch_identity(&self) -> &ClientLaunchIdentity {
        &self.launch_identity
    }

    fn next_request_id(&self) -> jsonrpc::Id {
        let id = self.request_counter.fetch_add(1, Ordering::Relaxed);
        jsonrpc::Id::Num(id)
    }

    fn value_into_params(value: Value) -> jsonrpc::Params {
        use jsonrpc::Params;

        match value {
            Value::Null => Params::None,
            Value::Bool(_) | Value::Number(_) | Value::String(_) => Params::Array(vec![value]),
            Value::Array(vec) => Params::Array(vec),
            Value::Object(map) => Params::Map(map),
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.capabilities.get().is_some()
    }

    pub fn capabilities(&self) -> &lsp::ServerCapabilities {
        self.capabilities
            .get()
            .expect("language server not yet initialized!")
    }

    pub(crate) fn file_operations_intests(&self) -> &FileOperationsInterest {
        self.file_operation_interest
            .get_or_init(|| FileOperationsInterest::new(self.capabilities()))
    }

    /// Client has to be initialized otherwise this function panics
    #[inline]
    pub fn supports_feature(&self, feature: LanguageServerFeature) -> bool {
        let capabilities = self.capabilities();

        use lsp::*;
        match feature {
            LanguageServerFeature::Format => matches!(
                capabilities.document_formatting_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoDeclaration => matches!(
                capabilities.declaration_provider,
                Some(
                    DeclarationCapability::Simple(true)
                        | DeclarationCapability::RegistrationOptions(_)
                        | DeclarationCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoDefinition => matches!(
                capabilities.definition_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoTypeDefinition => matches!(
                capabilities.type_definition_provider,
                Some(
                    TypeDefinitionProviderCapability::Simple(true)
                        | TypeDefinitionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoReference => matches!(
                capabilities.references_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoImplementation => matches!(
                capabilities.implementation_provider,
                Some(
                    ImplementationProviderCapability::Simple(true)
                        | ImplementationProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::SignatureHelp => capabilities.signature_help_provider.is_some(),
            LanguageServerFeature::Hover => matches!(
                capabilities.hover_provider,
                Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_),)
            ),
            LanguageServerFeature::DocumentHighlight => matches!(
                capabilities.document_highlight_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Completion => capabilities.completion_provider.is_some(),
            LanguageServerFeature::CodeAction => matches!(
                capabilities.code_action_provider,
                Some(
                    CodeActionProviderCapability::Simple(true)
                        | CodeActionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::WorkspaceCommand => {
                capabilities.execute_command_provider.is_some()
            }
            LanguageServerFeature::DocumentSymbols => matches!(
                capabilities.document_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::WorkspaceSymbols => matches!(
                capabilities.workspace_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Diagnostics => true, // there's no extra server capability
            LanguageServerFeature::PullDiagnostics => capabilities.diagnostic_provider.is_some(),
            LanguageServerFeature::RenameSymbol => matches!(
                capabilities.rename_provider,
                Some(OneOf::Left(true)) | Some(OneOf::Right(_))
            ),
            LanguageServerFeature::InlayHints => matches!(
                capabilities.inlay_hint_provider,
                Some(OneOf::Left(true) | OneOf::Right(InlayHintServerCapabilities::Options(_)))
            ),
            LanguageServerFeature::DocumentColors => matches!(
                capabilities.color_provider,
                Some(
                    ColorProviderCapability::Simple(true)
                        | ColorProviderCapability::ColorProvider(_)
                        | ColorProviderCapability::Options(_)
                )
            ),
            LanguageServerFeature::CodeLens => capabilities.code_lens_provider.is_some(),
            LanguageServerFeature::DocumentLinks => capabilities.document_link_provider.is_some(),
            LanguageServerFeature::FoldingRange => matches!(
                capabilities.folding_range_provider,
                Some(
                    lsp::FoldingRangeProviderCapability::Simple(true)
                        | lsp::FoldingRangeProviderCapability::FoldingProvider(_)
                        | lsp::FoldingRangeProviderCapability::Options(_)
                )
            ),
            LanguageServerFeature::SelectionRange => matches!(
                capabilities.selection_range_provider,
                Some(
                    lsp::SelectionRangeProviderCapability::Simple(true)
                        | lsp::SelectionRangeProviderCapability::Options(_)
                        | lsp::SelectionRangeProviderCapability::RegistrationOptions(_)
                )
            ),
            LanguageServerFeature::LinkedEditingRange => matches!(
                capabilities.linked_editing_range_provider,
                Some(
                    lsp::LinkedEditingRangeServerCapabilities::Simple(true)
                        | lsp::LinkedEditingRangeServerCapabilities::Options(_)
                        | lsp::LinkedEditingRangeServerCapabilities::RegistrationOptions(_)
                )
            ),
            LanguageServerFeature::OnTypeFormatting => {
                capabilities.document_on_type_formatting_provider.is_some()
            }
        }
    }

    fn semantic_tokens_options(
        capabilities: &lsp::SemanticTokensServerCapabilities,
    ) -> &lsp::SemanticTokensOptions {
        match capabilities {
            lsp::SemanticTokensServerCapabilities::SemanticTokensOptions(options) => options,
            lsp::SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(options) => {
                &options.semantic_tokens_options
            }
        }
    }

    pub fn supports_semantic_tokens_full(&self) -> bool {
        self.capabilities()
            .semantic_tokens_provider
            .as_ref()
            .and_then(|capabilities| Self::semantic_tokens_options(capabilities).full.as_ref())
            .is_some_and(|options| match options {
                lsp::SemanticTokensFullOptions::Bool(supported) => *supported,
                lsp::SemanticTokensFullOptions::Delta { .. } => true,
            })
    }

    pub fn supports_semantic_tokens_delta(&self) -> bool {
        self.capabilities()
            .semantic_tokens_provider
            .as_ref()
            .and_then(|capabilities| Self::semantic_tokens_options(capabilities).full.as_ref())
            .is_some_and(|options| {
                matches!(
                    options,
                    lsp::SemanticTokensFullOptions::Delta { delta: Some(true) }
                )
            })
    }

    pub fn semantic_tokens_delta_previous_result_id(
        supports_delta: bool,
        previous_result_id: Option<&str>,
    ) -> Option<String> {
        supports_delta
            .then_some(previous_result_id?)
            .map(ToOwned::to_owned)
    }

    pub fn supports_semantic_tokens_range(&self) -> bool {
        self.capabilities()
            .semantic_tokens_provider
            .as_ref()
            .and_then(|capabilities| Self::semantic_tokens_options(capabilities).range)
            .unwrap_or(false)
    }

    pub fn semantic_tokens_legend(&self) -> Option<&lsp::SemanticTokensLegend> {
        self.capabilities()
            .semantic_tokens_provider
            .as_ref()
            .map(|capabilities| &Self::semantic_tokens_options(capabilities).legend)
    }

    pub fn supports_inline_completion(&self) -> bool {
        matches!(
            self.capabilities().inline_completion_provider,
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
        )
    }

    pub fn supports_inline_values(&self) -> bool {
        matches!(
            self.capabilities().inline_value_provider,
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
        )
    }

    pub fn offset_encoding(&self) -> OffsetEncoding {
        self.capabilities()
            .position_encoding
            .as_ref()
            .and_then(|encoding| match encoding.as_str() {
                "utf-8" => Some(OffsetEncoding::Utf8),
                "utf-16" => Some(OffsetEncoding::Utf16),
                "utf-32" => Some(OffsetEncoding::Utf32),
                encoding => {
                    log::error!(
                        "Server provided invalid position encoding {encoding}, defaulting to utf-16"
                    );
                    None
                }
            })
            .unwrap_or_default()
    }

    pub fn config(&self) -> Option<&Value> {
        self.config.as_ref()
    }

    pub fn workspace_folders(&self) -> Arc<Vec<lsp::WorkspaceFolder>> {
        self.workspace_folders.snapshot()
    }

    /// Execute a RPC request on the language server.
    fn call<R: lsp::request::Request>(
        &self,
        params: R::Params,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize + Send + 'static,
    {
        self.call_with_timeout::<R>(params, self.req_timeout)
    }

    fn call_with_ref<R: lsp::request::Request>(
        &self,
        params: &R::Params,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize + Clone + Send + 'static,
    {
        self.call::<R>(params.clone())
    }

    fn call_with_timeout<R: lsp::request::Request>(
        &self,
        params: R::Params,
        timeout_secs: u64,
    ) -> impl Future<Output = Result<R::Result>>
    where
        R::Params: serde::Serialize + Send + 'static,
    {
        let transport = self.transport.clone();
        let id = self.next_request_id();
        let request_id = id.clone();

        async move {
            use std::time::Duration;
            transport
                .request_deferred(
                    id,
                    R::METHOD,
                    move || {
                        let params = serde_json::to_value(params)?;
                        serde_json::to_string(&jsonrpc::MethodCall {
                            jsonrpc: Some(jsonrpc::Version::V2),
                            id: request_id,
                            method: R::METHOD.to_string(),
                            params: Self::value_into_params(params),
                        })
                    },
                    Duration::from_secs(timeout_secs),
                )
                .await
                .and_then(|value| serde_json::from_value(value).map_err(Into::into))
        }
    }

    /// Execute a dynamically named request while retaining normal timeout and
    /// request-lease cancellation semantics.
    pub fn call_custom(
        &self,
        method: String,
        params: Value,
    ) -> impl Future<Output = Result<Value>> {
        let transport = self.transport.clone();
        let id = self.next_request_id();
        let timeout = self.req_timeout;
        let request = jsonrpc::MethodCall {
            jsonrpc: Some(jsonrpc::Version::V2),
            id,
            method,
            params: Self::value_into_params(params),
        };
        async move {
            transport
                .request(request, std::time::Duration::from_secs(timeout))
                .await
        }
    }

    /// Send a RPC notification to the language server.
    pub fn notify<R: lsp::notification::Notification>(&self, params: R::Params)
    where
        R::Params: serde::Serialize + Send + 'static,
    {
        self.notify_deferred::<R, _>(move || params);
    }

    fn notify_deferred<R, F>(&self, build_params: F)
    where
        R: lsp::notification::Notification,
        R::Params: serde::Serialize,
        F: FnOnce() -> R::Params + Send + 'static,
    {
        let payload = Payload::deferred_notification(R::METHOD, move || {
            let params = serde_json::to_value(build_params())?;
            let notification = jsonrpc::Notification {
                jsonrpc: Some(jsonrpc::Version::V2),
                method: R::METHOD.to_string(),
                params: Self::value_into_params(params),
            };
            serde_json::to_string(&notification)
        });
        match self.transport.send(payload) {
            Ok(()) => {}
            Err(Error::OutboundQueueFull) => log::error!(
                "language-server outbound queue saturated; rejected notification '{}' for server '{}'",
                R::METHOD,
                self.name,
            ),
            Err(Error::StreamClosed) => log::debug!(
                "Discarded notification '{}' because language server '{}' is closed",
                R::METHOD,
                self.name,
            ),
            Err(error) => log::error!(
                "Failed to enqueue notification '{}' for language server '{}': {error}",
                R::METHOD,
                self.name,
            ),
        }
    }

    fn update_document(&self, update: DocumentUpdate) {
        match self.transport.send(Payload::DocumentUpdate(update)) {
            Ok(()) => {}
            Err(Error::StreamClosed) => log::debug!(
                "discarded document synchronization because language server '{}' is closed",
                self.name,
            ),
            Err(error) => log::error!(
                "failed to update document synchronization for language server '{}': {error}",
                self.name,
            ),
        }
    }

    /// Reply to a language server RPC call.
    pub fn reply(
        &self,
        id: jsonrpc::Id,
        result: core::result::Result<Value, jsonrpc::Error>,
    ) -> Result<()> {
        self.transport.reply(Self::reply_output(id, result))
    }

    /// Reply and wait until the response has been written to the server stream.
    pub async fn reply_async(
        &self,
        id: jsonrpc::Id,
        result: core::result::Result<Value, jsonrpc::Error>,
    ) -> Result<()> {
        self.transport
            .reply_async(Self::reply_output(id, result))
            .await
    }

    fn reply_output(
        id: jsonrpc::Id,
        result: core::result::Result<Value, jsonrpc::Error>,
    ) -> jsonrpc::Output {
        use jsonrpc::{Failure, Output, Success, Version};

        match result {
            Ok(result) => Output::Success(Success {
                jsonrpc: Some(Version::V2),
                id,
                result,
            }),
            Err(error) => Output::Failure(Failure {
                jsonrpc: Some(Version::V2),
                id,
                error,
            }),
        }
    }

    // -------------------------------------------------------------------------------------------
    // General messages
    // -------------------------------------------------------------------------------------------

    pub(crate) async fn finish_initialization(&self) -> Result<()> {
        self.transport.initialized().await
    }

    pub(crate) fn fail_initialization(&self, error: impl Into<Arc<str>>) {
        self.transport.fail_initialization(error);
    }

    pub(crate) async fn initialize(&self, enable_snippets: bool) -> Result<lsp::InitializeResult> {
        if let Some(config) = &self.config {
            log::info!("Using custom LSP config: {}", config);
        }

        #[allow(deprecated)]
        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            workspace_folders: Some((*self.workspace_folders.snapshot()).clone()),
            // root_path is obsolete, but some clients like pyright still use it so we specify both.
            // clients will prefer _uri if possible
            root_path: self.root_path.to_str().map(|path| path.to_owned()),
            root_uri: self.root_uri.clone(),
            initialization_options: self.config.clone(),
            capabilities: lsp::ClientCapabilities {
                workspace: Some(lsp::WorkspaceClientCapabilities {
                    configuration: Some(true),
                    did_change_configuration: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    workspace_folders: Some(true),
                    apply_edit: Some(true),
                    symbol: Some(lsp::WorkspaceSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    execute_command: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    inlay_hint: Some(lsp::InlayHintWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    semantic_tokens: Some(lsp::SemanticTokensWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    inline_value: Some(lsp::InlineValueWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    code_lens: Some(lsp::CodeLensWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    workspace_edit: Some(lsp::WorkspaceEditClientCapabilities {
                        document_changes: Some(true),
                        resource_operations: Some(vec![
                            lsp::ResourceOperationKind::Create,
                            lsp::ResourceOperationKind::Rename,
                            lsp::ResourceOperationKind::Delete,
                        ]),
                        failure_handling: Some(lsp::FailureHandlingKind::Abort),
                        normalizes_line_endings: Some(false),
                        change_annotation_support: None,
                    }),
                    did_change_watched_files: Some(lsp::DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(true),
                        relative_pattern_support: Some(true),
                    }),
                    file_operations: Some(lsp::WorkspaceFileOperationsClientCapabilities {
                        dynamic_registration: None,
                        will_create: Some(true),
                        did_create: Some(true),
                        will_rename: Some(true),
                        did_rename: Some(true),
                        will_delete: Some(true),
                        did_delete: Some(true),
                    }),
                    diagnostic: Some(lsp::DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                }),
                text_document: Some(lsp::TextDocumentClientCapabilities {
                    completion: Some(lsp::CompletionClientCapabilities {
                        completion_item: Some(lsp::CompletionItemCapability {
                            snippet_support: Some(enable_snippets),
                            resolve_support: Some(lsp::CompletionItemCapabilityResolveSupport {
                                properties: vec![
                                    String::from("documentation"),
                                    String::from("detail"),
                                    String::from("additionalTextEdits"),
                                    String::from("commitCharacters"),
                                ],
                            }),
                            insert_replace_support: Some(true),
                            label_details_support: Some(true),
                            deprecated_support: Some(true),
                            tag_support: Some(lsp::TagSupport {
                                value_set: vec![lsp::CompletionItemTag::DEPRECATED],
                            }),
                            ..Default::default()
                        }),
                        completion_item_kind: Some(lsp::CompletionItemKindCapability {
                            ..Default::default()
                        }),
                        context_support: None, // additional context information Some(true)
                        ..Default::default()
                    }),
                    hover: Some(lsp::HoverClientCapabilities {
                        // if not specified, rust-analyzer returns plaintext marked as markdown but
                        // badly formatted.
                        content_format: Some(vec![lsp::MarkupKind::Markdown]),
                        ..Default::default()
                    }),
                    signature_help: Some(lsp::SignatureHelpClientCapabilities {
                        signature_information: Some(lsp::SignatureInformationSettings {
                            documentation_format: Some(vec![lsp::MarkupKind::Markdown]),
                            parameter_information: Some(lsp::ParameterInformationSettings {
                                label_offset_support: Some(true),
                            }),
                            active_parameter_support: Some(true),
                        }),
                        context_support: Some(true),
                        ..Default::default()
                    }),
                    rename: Some(lsp::RenameClientCapabilities {
                        dynamic_registration: Some(false),
                        prepare_support: Some(true),
                        prepare_support_default_behavior: None,
                        honors_change_annotations: Some(false),
                    }),
                    formatting: Some(lsp::DocumentFormattingClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp::CodeActionClientCapabilities {
                        code_action_literal_support: Some(lsp::CodeActionLiteralSupport {
                            code_action_kind: lsp::CodeActionKindLiteralSupport {
                                value_set: [
                                    lsp::CodeActionKind::EMPTY,
                                    lsp::CodeActionKind::QUICKFIX,
                                    lsp::CodeActionKind::REFACTOR,
                                    lsp::CodeActionKind::REFACTOR_EXTRACT,
                                    lsp::CodeActionKind::REFACTOR_INLINE,
                                    lsp::CodeActionKind::REFACTOR_REWRITE,
                                    lsp::CodeActionKind::SOURCE,
                                    lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                    lsp::CodeActionKind::SOURCE_FIX_ALL,
                                ]
                                .iter()
                                .map(|kind| kind.as_str().to_string())
                                .collect(),
                            },
                        }),
                        is_preferred_support: Some(true),
                        disabled_support: Some(true),
                        data_support: Some(true),
                        resolve_support: Some(CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_owned(), "command".to_owned()],
                        }),
                        ..Default::default()
                    }),
                    diagnostic: Some(lsp::DiagnosticClientCapabilities {
                        dynamic_registration: Some(false),
                        related_document_support: Some(true),
                    }),
                    publish_diagnostics: Some(lsp::PublishDiagnosticsClientCapabilities {
                        version_support: Some(true),
                        tag_support: Some(lsp::TagSupport {
                            value_set: vec![
                                lsp::DiagnosticTag::UNNECESSARY,
                                lsp::DiagnosticTag::DEPRECATED,
                                lsp::DiagnosticTag::UNNEEDED_PARENTHESES,
                            ],
                        }),
                        ..Default::default()
                    }),
                    inlay_hint: Some(lsp::InlayHintClientCapabilities {
                        dynamic_registration: Some(false),
                        resolve_support: Some(lsp::InlayHintResolveClientCapabilities {
                            properties: vec!["tooltip".to_owned(), "label.tooltip".to_owned()],
                        }),
                    }),
                    code_lens: Some(lsp::CodeLensClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_link: Some(lsp::DocumentLinkClientCapabilities {
                        dynamic_registration: Some(false),
                        tooltip_support: Some(true),
                    }),
                    folding_range: Some(lsp::FoldingRangeClientCapabilities {
                        dynamic_registration: Some(false),
                        line_folding_only: Some(false),
                        folding_range_kind: Some(lsp::FoldingRangeKindCapability {
                            value_set: Some(vec![
                                lsp::FoldingRangeKind::Comment,
                                lsp::FoldingRangeKind::Imports,
                                lsp::FoldingRangeKind::Region,
                            ]),
                        }),
                        ..Default::default()
                    }),
                    selection_range: Some(lsp::SelectionRangeClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    linked_editing_range: Some(lsp::LinkedEditingRangeClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    call_hierarchy: Some(lsp::CallHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    type_hierarchy: Some(lsp::TypeHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    semantic_tokens: Some(lsp::SemanticTokensClientCapabilities {
                        dynamic_registration: Some(false),
                        requests: lsp::SemanticTokensClientCapabilitiesRequests {
                            range: Some(true),
                            full: Some(lsp::SemanticTokensFullOptions::Delta { delta: Some(true) }),
                        },
                        token_types: vec![
                            lsp::SemanticTokenType::NAMESPACE,
                            lsp::SemanticTokenType::TYPE,
                            lsp::SemanticTokenType::CLASS,
                            lsp::SemanticTokenType::ENUM,
                            lsp::SemanticTokenType::INTERFACE,
                            lsp::SemanticTokenType::STRUCT,
                            lsp::SemanticTokenType::TYPE_PARAMETER,
                            lsp::SemanticTokenType::PARAMETER,
                            lsp::SemanticTokenType::VARIABLE,
                            lsp::SemanticTokenType::PROPERTY,
                            lsp::SemanticTokenType::ENUM_MEMBER,
                            lsp::SemanticTokenType::EVENT,
                            lsp::SemanticTokenType::FUNCTION,
                            lsp::SemanticTokenType::METHOD,
                            lsp::SemanticTokenType::MACRO,
                            lsp::SemanticTokenType::KEYWORD,
                            lsp::SemanticTokenType::MODIFIER,
                            lsp::SemanticTokenType::COMMENT,
                            lsp::SemanticTokenType::STRING,
                            lsp::SemanticTokenType::NUMBER,
                            lsp::SemanticTokenType::REGEXP,
                            lsp::SemanticTokenType::OPERATOR,
                            lsp::SemanticTokenType::DECORATOR,
                        ],
                        token_modifiers: vec![
                            lsp::SemanticTokenModifier::DECLARATION,
                            lsp::SemanticTokenModifier::DEFINITION,
                            lsp::SemanticTokenModifier::READONLY,
                            lsp::SemanticTokenModifier::STATIC,
                            lsp::SemanticTokenModifier::DEPRECATED,
                            lsp::SemanticTokenModifier::ABSTRACT,
                            lsp::SemanticTokenModifier::ASYNC,
                            lsp::SemanticTokenModifier::MODIFICATION,
                            lsp::SemanticTokenModifier::DOCUMENTATION,
                            lsp::SemanticTokenModifier::DEFAULT_LIBRARY,
                        ],
                        formats: vec![lsp::TokenFormat::RELATIVE],
                        overlapping_token_support: Some(false),
                        multiline_token_support: Some(true),
                        server_cancel_support: Some(true),
                        augments_syntax_tokens: Some(true),
                    }),
                    inline_value: Some(lsp::InlineValueClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    inline_completion: Some(lsp::InlineCompletionClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    on_type_formatting: Some(lsp::DocumentOnTypeFormattingClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    ..Default::default()
                }),
                window: Some(lsp::WindowClientCapabilities {
                    show_message: Some(lsp::ShowMessageRequestClientCapabilities {
                        message_action_item: Some(lsp::MessageActionItemCapabilities {
                            additional_properties_support: Some(true),
                        }),
                    }),
                    work_done_progress: Some(true),
                    show_document: Some(lsp::ShowDocumentClientCapabilities { support: true }),
                }),
                general: Some(lsp::GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF32,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            trace: None,
            client_info: Some(lsp::ClientInfo {
                name: String::from("helix"),
                version: Some(String::from(VERSION_AND_GIT_HASH)),
            }),
            locale: client_locale(),
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
        };

        self.call::<lsp::request::Initialize>(params).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.call::<lsp::request::Shutdown>(()).await
    }

    pub fn exit(&self) {
        self.notify::<lsp::notification::Exit>(())
    }

    /// Tries to shut down the language server but returns
    /// early if server responds with an error.
    pub async fn shutdown_and_exit(&self) -> Result<()> {
        self.shutdown().await?;
        self.exit();
        Ok(())
    }

    /// Forcefully shuts down the language server ignoring any errors.
    pub async fn force_shutdown(&self) -> Result<()> {
        if let Err(e) = self.shutdown().await {
            log::warn!("language server failed to terminate gracefully - {}", e);
        }
        self.exit();
        Ok(())
    }

    pub async fn force_kill(&self) -> Result<()> {
        self.process.lock().await.kill().await?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<std::process::ExitStatus> {
        self.process.lock().await.wait().await.map_err(Into::into)
    }

    // -------------------------------------------------------------------------------------------
    // Workspace
    // -------------------------------------------------------------------------------------------

    pub fn did_change_configuration(&self, settings: Value) {
        self.notify::<lsp::notification::DidChangeConfiguration>(
            lsp::DidChangeConfigurationParams { settings },
        )
    }

    pub fn did_change_workspace(&self, added: Vec<WorkspaceFolder>, removed: Vec<WorkspaceFolder>) {
        self.notify::<DidChangeWorkspaceFolders>(DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent { added, removed },
        })
    }

    pub fn will_create(
        &self,
        path: &Path,
        is_dir: bool,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceEdit>>>> {
        let capabilities = self.file_operations_intests();
        if !capabilities.will_create.has_interest(path, is_dir) {
            return None;
        }
        let files = vec![lsp::FileCreate {
            uri: file_operation_uri(path, is_dir)?,
        }];
        Some(self.call_with_timeout::<lsp::request::WillCreateFiles>(
            lsp::CreateFilesParams { files },
            5,
        ))
    }

    pub fn did_create(&self, path: &Path, is_dir: bool) -> Option<()> {
        let capabilities = self.file_operations_intests();
        if !capabilities.did_create.has_interest(path, is_dir) {
            return None;
        }
        let files = vec![lsp::FileCreate {
            uri: file_operation_uri(path, is_dir)?,
        }];
        self.notify::<lsp::notification::DidCreateFiles>(lsp::CreateFilesParams { files });
        Some(())
    }

    pub fn will_rename(
        &self,
        old_path: &Path,
        new_path: &Path,
        is_dir: bool,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceEdit>>>> {
        let capabilities = self.file_operations_intests();
        if !capabilities.will_rename.has_interest(old_path, is_dir) {
            return None;
        }
        let files = vec![lsp::FileRename {
            old_uri: file_operation_uri(old_path, is_dir)?,
            new_uri: file_operation_uri(new_path, is_dir)?,
        }];
        Some(self.call_with_timeout::<lsp::request::WillRenameFiles>(
            lsp::RenameFilesParams { files },
            5,
        ))
    }

    pub fn did_rename(&self, old_path: &Path, new_path: &Path, is_dir: bool) -> Option<()> {
        let capabilities = self.file_operations_intests();
        if !capabilities.did_rename.has_interest(new_path, is_dir) {
            return None;
        }

        let files = vec![lsp::FileRename {
            old_uri: file_operation_uri(old_path, is_dir)?,
            new_uri: file_operation_uri(new_path, is_dir)?,
        }];
        self.notify::<lsp::notification::DidRenameFiles>(lsp::RenameFilesParams { files });
        Some(())
    }

    pub fn will_delete(
        &self,
        path: &Path,
        is_dir: bool,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceEdit>>>> {
        let capabilities = self.file_operations_intests();
        if !capabilities.will_delete.has_interest(path, is_dir) {
            return None;
        }
        let files = vec![lsp::FileDelete {
            uri: file_operation_uri(path, is_dir)?,
        }];
        Some(self.call_with_timeout::<lsp::request::WillDeleteFiles>(
            lsp::DeleteFilesParams { files },
            5,
        ))
    }

    pub fn did_delete(&self, path: &Path, is_dir: bool) -> Option<()> {
        let capabilities = self.file_operations_intests();
        if !capabilities.did_delete.has_interest(path, is_dir) {
            return None;
        }
        let files = vec![lsp::FileDelete {
            uri: file_operation_uri(path, is_dir)?,
        }];
        self.notify::<lsp::notification::DidDeleteFiles>(lsp::DeleteFilesParams { files });
        Some(())
    }

    // -------------------------------------------------------------------------------------------
    // Text document
    // -------------------------------------------------------------------------------------------

    pub fn text_document_did_open(
        &self,
        uri: lsp::Url,
        version: i32,
        doc: &Rope,
        language_id: String,
    ) {
        self.update_document(DocumentUpdate::Open {
            uri,
            version,
            text: doc.clone(),
            language_id,
        });
    }

    pub fn changeset_to_changes(
        old_text: &Rope,
        new_text: &Rope,
        changeset: &ChangeSet,
        offset_encoding: OffsetEncoding,
    ) -> Vec<lsp::TextDocumentContentChangeEvent> {
        let mut iter = changeset.changes().iter().peekable();
        let mut old_pos = 0;
        let mut new_pos = 0;

        let mut changes = Vec::new();

        use crate::util::pos_to_lsp_pos;
        use helix_core::Operation::*;

        // this is dumb. TextEdit describes changes to the initial doc (concurrent), but
        // TextDocumentContentChangeEvent describes a series of changes (sequential).
        // So S -> S1 -> S2, meaning positioning depends on the previous edits.
        //
        // Calculation is therefore a bunch trickier.

        use helix_core::RopeSlice;
        fn traverse(
            pos: lsp::Position,
            text: RopeSlice,
            offset_encoding: OffsetEncoding,
        ) -> lsp::Position {
            let lsp::Position {
                mut line,
                mut character,
            } = pos;

            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                // LSP only considers \n, \r or \r\n as line endings
                if ch == '\n' || ch == '\r' {
                    // consume a \r\n
                    if ch == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    line += 1;
                    character = 0;
                } else {
                    character += match offset_encoding {
                        OffsetEncoding::Utf8 => ch.len_utf8() as u32,
                        OffsetEncoding::Utf16 => ch.len_utf16() as u32,
                        OffsetEncoding::Utf32 => 1,
                    };
                }
            }
            lsp::Position { line, character }
        }

        let old_text = old_text.slice(..);

        while let Some(change) = iter.next() {
            let len = match change {
                Delete(i) | Retain(i) => *i,
                Insert(_) => 0,
            };
            let mut old_end = old_pos + len;

            match change {
                Retain(i) => {
                    new_pos += i;
                }
                Delete(_) => {
                    let start = pos_to_lsp_pos(new_text, new_pos, offset_encoding);
                    let end = traverse(start, old_text.slice(old_pos..old_end), offset_encoding);

                    // deletion
                    changes.push(lsp::TextDocumentContentChangeEvent {
                        range: Some(lsp::Range::new(start, end)),
                        text: "".to_string(),
                        range_length: None,
                    });
                }
                Insert(s) => {
                    let start = pos_to_lsp_pos(new_text, new_pos, offset_encoding);

                    new_pos += s.chars().count();

                    // a subsequent delete means a replace, consume it
                    let end = if let Some(Delete(len)) = iter.peek() {
                        old_end = old_pos + len;
                        let end =
                            traverse(start, old_text.slice(old_pos..old_end), offset_encoding);

                        iter.next();

                        // replacement
                        end
                    } else {
                        // insert
                        start
                    };

                    changes.push(lsp::TextDocumentContentChangeEvent {
                        range: Some(lsp::Range::new(start, end)),
                        text: s.to_string(),
                        range_length: None,
                    });
                }
            }
            old_pos = old_end;
        }

        changes
    }

    pub fn text_document_did_change(
        &self,
        text_document: lsp::VersionedTextDocumentIdentifier,
        new_text: &Rope,
    ) -> Option<()> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support document sync.
        let sync_capabilities = match capabilities.text_document_sync {
            Some(
                lsp::TextDocumentSyncCapability::Kind(kind)
                | lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                    change: Some(kind),
                    ..
                }),
            ) => kind,
            // None | SyncOptions { changes: None }
            _ => return None,
        };

        match sync_capabilities {
            lsp::TextDocumentSyncKind::FULL | lsp::TextDocumentSyncKind::INCREMENTAL => {}
            lsp::TextDocumentSyncKind::NONE => return None,
            kind => {
                log::error!(
                    "language server '{}' advertised unsupported document sync kind {kind:?}",
                    self.name
                );
                return None;
            }
        }

        let update = DocumentUpdate::Change(DocumentChangeTarget {
            text_document,
            new_text: new_text.clone(),
            sync_kind: sync_capabilities,
            offset_encoding: self.offset_encoding(),
        });
        self.update_document(update);
        Some(())
    }

    pub fn text_document_did_close(&self, text_document: lsp::TextDocumentIdentifier) {
        self.update_document(DocumentUpdate::Close {
            uri: text_document.uri,
        });
    }

    // will_save / will_save_wait_until

    pub fn text_document_did_save(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        text: &Rope,
    ) -> Option<()> {
        let capabilities = self.capabilities.get().unwrap();

        let include_text = match &capabilities.text_document_sync.as_ref()? {
            lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                save: options,
                ..
            }) => match options.as_ref()? {
                lsp::TextDocumentSyncSaveOptions::Supported(true) => false,
                lsp::TextDocumentSyncSaveOptions::SaveOptions(lsp::SaveOptions {
                    include_text,
                }) => include_text.unwrap_or(false),
                lsp::TextDocumentSyncSaveOptions::Supported(false) => return None,
            },
            // see: https://github.com/microsoft/language-server-protocol/issues/288
            lsp::TextDocumentSyncCapability::Kind(..) => false,
        };

        self.update_document(DocumentUpdate::Save {
            uri: text_document.uri,
            text: text.clone(),
            include_text,
        });
        Some(())
    }

    pub fn completion(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
        context: lsp::CompletionContext,
    ) -> Option<impl Future<Output = Result<Option<lsp::CompletionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support completion.
        capabilities.completion_provider.as_ref()?;

        let params = lsp::CompletionParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: Some(context),
            // TODO: support these tokens by async receiving and updating the choice list
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::Completion>(params))
    }

    pub fn resolve_completion_item(
        &self,
        completion_item: &lsp::CompletionItem,
    ) -> impl Future<Output = Result<lsp::CompletionItem>> {
        self.call_with_ref::<lsp::request::ResolveCompletionItem>(completion_item)
    }

    pub fn resolve_code_action(
        &self,
        code_action: &lsp::CodeAction,
    ) -> Option<impl Future<Output = Result<lsp::CodeAction>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support resolving code actions.
        match capabilities.code_action_provider {
            Some(lsp::CodeActionProviderCapability::Options(lsp::CodeActionOptions {
                resolve_provider: Some(true),
                ..
            })) => (),
            _ => return None,
        }

        Some(self.call_with_ref::<lsp::request::CodeActionResolveRequest>(code_action))
    }

    pub fn text_document_signature_help(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
        context: Option<lsp::SignatureHelpContext>,
    ) -> Option<impl Future<Output = Result<Option<SignatureHelp>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support signature help.
        capabilities.signature_help_provider.as_ref()?;

        let params = lsp::SignatureHelpParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            context,
        };

        Some(self.call::<lsp::request::SignatureHelpRequest>(params))
    }

    pub fn text_document_range_inlay_hints(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::InlayHint>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.inlay_hint_provider {
            Some(
                lsp::OneOf::Left(true)
                | lsp::OneOf::Right(lsp::InlayHintServerCapabilities::Options(_)),
            ) => (),
            _ => return None,
        }

        let params = lsp::InlayHintParams {
            text_document,
            range,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::InlayHintRequest>(params))
    }

    pub fn text_document_semantic_tokens_full(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::SemanticTokensResult>>>> {
        if !self.supports_semantic_tokens_full() {
            return None;
        }

        let params = lsp::SemanticTokensParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::SemanticTokensFullRequest>(params))
    }

    pub fn text_document_semantic_tokens_full_delta(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        previous_result_id: String,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::SemanticTokensFullDeltaResult>>>> {
        if !self.supports_semantic_tokens_delta() {
            return None;
        }

        let params = lsp::SemanticTokensDeltaParams {
            text_document,
            previous_result_id,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::SemanticTokensFullDeltaRequest>(params))
    }

    pub fn text_document_semantic_tokens_range(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::SemanticTokensRangeResult>>>> {
        if !self.supports_semantic_tokens_range() {
            return None;
        }

        let params = lsp::SemanticTokensRangeParams {
            text_document,
            range,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::SemanticTokensRangeRequest>(params))
    }

    pub fn text_document_inline_completion(
        &self,
        text_document_position: lsp::TextDocumentPositionParams,
        context: lsp::InlineCompletionContext,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::InlineCompletionResponse>>>> {
        if !self.supports_inline_completion() {
            return None;
        }

        let params = lsp::InlineCompletionParams {
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            text_document_position,
            context,
        };

        Some(self.call::<lsp::request::InlineCompletionRequest>(params))
    }

    pub fn text_document_inline_values(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        context: lsp::InlineValueContext,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::InlineValue>>>>> {
        if !self.supports_inline_values() {
            return None;
        }

        let params = lsp::InlineValueParams {
            text_document,
            range,
            context,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::InlineValueRequest>(params))
    }

    pub fn resolve_inlay_hint(
        &self,
        hint: &lsp::InlayHint,
    ) -> Option<impl Future<Output = Result<lsp::InlayHint>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.inlay_hint_provider {
            Some(lsp::OneOf::Right(lsp::InlayHintServerCapabilities::Options(
                lsp::InlayHintOptions {
                    resolve_provider: Some(true),
                    ..
                },
            ))) => (),
            _ => return None,
        }

        Some(self.call_with_ref::<lsp::request::InlayHintResolveRequest>(hint))
    }

    pub fn text_document_code_lens(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CodeLens>>>>> {
        self.capabilities
            .get()
            .unwrap()
            .code_lens_provider
            .as_ref()?;
        let params = lsp::CodeLensParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: helix_lsp_types::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::CodeLensRequest>(params))
    }

    pub fn resolve_code_lens(
        &self,
        code_lens: &lsp::CodeLens,
    ) -> Option<impl Future<Output = Result<lsp::CodeLens>>> {
        match self.capabilities.get().unwrap().code_lens_provider {
            Some(lsp::CodeLensOptions {
                resolve_provider: Some(true),
            }) => (),
            _ => return None,
        }

        Some(self.call_with_ref::<lsp::request::CodeLensResolve>(code_lens))
    }

    pub fn text_document_document_link(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::DocumentLink>>>>> {
        self.capabilities
            .get()
            .unwrap()
            .document_link_provider
            .as_ref()?;
        let params = lsp::DocumentLinkParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: helix_lsp_types::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::DocumentLinkRequest>(params))
    }

    pub fn resolve_document_link(
        &self,
        document_link: &lsp::DocumentLink,
    ) -> Option<impl Future<Output = Result<lsp::DocumentLink>>> {
        match self.capabilities.get().unwrap().document_link_provider {
            Some(lsp::DocumentLinkOptions {
                resolve_provider: Some(true),
                ..
            }) => (),
            _ => return None,
        }

        Some(self.call_with_ref::<lsp::request::DocumentLinkResolve>(document_link))
    }

    pub fn text_document_folding_range(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::FoldingRange>>>>> {
        match self.capabilities.get().unwrap().folding_range_provider {
            Some(
                lsp::FoldingRangeProviderCapability::Simple(true)
                | lsp::FoldingRangeProviderCapability::FoldingProvider(_)
                | lsp::FoldingRangeProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }
        let params = lsp::FoldingRangeParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: helix_lsp_types::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::FoldingRangeRequest>(params))
    }

    pub fn text_document_linked_editing_range(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::LinkedEditingRanges>>>> {
        match self
            .capabilities
            .get()
            .unwrap()
            .linked_editing_range_provider
        {
            Some(
                lsp::LinkedEditingRangeServerCapabilities::Simple(true)
                | lsp::LinkedEditingRangeServerCapabilities::Options(_)
                | lsp::LinkedEditingRangeServerCapabilities::RegistrationOptions(_),
            ) => (),
            _ => return None,
        }
        let params = lsp::LinkedEditingRangeParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::LinkedEditingRange>(params))
    }

    pub fn text_document_selection_range(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        positions: Vec<lsp::Position>,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::SelectionRange>>>>> {
        match self.capabilities.get().unwrap().selection_range_provider {
            Some(
                lsp::SelectionRangeProviderCapability::Simple(true)
                | lsp::SelectionRangeProviderCapability::Options(_)
                | lsp::SelectionRangeProviderCapability::RegistrationOptions(_),
            ) => (),
            _ => return None,
        }
        let params = lsp::SelectionRangeParams {
            text_document,
            positions,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: Default::default(),
        };

        Some(self.call::<lsp::request::SelectionRangeRequest>(params))
    }

    pub fn text_document_prepare_call_hierarchy(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyItem>>>>> {
        match self.capabilities.get().unwrap().call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyPrepareParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::CallHierarchyPrepare>(params))
    }

    pub fn call_hierarchy_incoming_calls(
        &self,
        item: lsp::CallHierarchyItem,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyIncomingCall>>>>> {
        match self.capabilities.get().unwrap().call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::CallHierarchyIncomingCalls>(params))
    }

    pub fn call_hierarchy_outgoing_calls(
        &self,
        item: lsp::CallHierarchyItem,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CallHierarchyOutgoingCall>>>>> {
        match self.capabilities.get().unwrap().call_hierarchy_provider {
            Some(
                lsp::CallHierarchyServerCapability::Simple(true)
                | lsp::CallHierarchyServerCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::CallHierarchyOutgoingCalls>(params))
    }

    pub fn text_document_prepare_type_hierarchy(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TypeHierarchyItem>>>>> {
        match self.capabilities.get().unwrap().type_hierarchy_provider {
            Some(
                lsp::TypeHierarchyServerCapability::Simple(true)
                | lsp::TypeHierarchyServerCapability::Options(_)
                | lsp::TypeHierarchyServerCapability::RegistrationOptions(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::TypeHierarchyPrepareParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::TypeHierarchyPrepare>(params))
    }

    pub fn type_hierarchy_supertypes(
        &self,
        item: lsp::TypeHierarchyItem,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TypeHierarchyItem>>>>> {
        match self.capabilities.get().unwrap().type_hierarchy_provider {
            Some(
                lsp::TypeHierarchyServerCapability::Simple(true)
                | lsp::TypeHierarchyServerCapability::Options(_)
                | lsp::TypeHierarchyServerCapability::RegistrationOptions(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::TypeHierarchySupertypesParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::TypeHierarchySupertypes>(params))
    }

    pub fn type_hierarchy_subtypes(
        &self,
        item: lsp::TypeHierarchyItem,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TypeHierarchyItem>>>>> {
        match self.capabilities.get().unwrap().type_hierarchy_provider {
            Some(
                lsp::TypeHierarchyServerCapability::Simple(true)
                | lsp::TypeHierarchyServerCapability::Options(_)
                | lsp::TypeHierarchyServerCapability::RegistrationOptions(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::TypeHierarchySubtypesParams {
            item,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::TypeHierarchySubtypes>(params))
    }

    pub fn text_document_on_type_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        ch: String,
        options: lsp::FormattingOptions,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TextEdit>>>>> {
        let capabilities = self.capabilities.get().unwrap();
        let on_type = capabilities.document_on_type_formatting_provider.as_ref()?;
        if on_type.first_trigger_character != ch
            && !on_type
                .more_trigger_character
                .as_ref()
                .is_some_and(|chars| chars.iter().any(|trigger| trigger == &ch))
        {
            return None;
        }

        let params = lsp::DocumentOnTypeFormattingParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            ch,
            options: self.get_merged_formatting_options(options),
        };

        Some(self.call::<lsp::request::OnTypeFormatting>(params))
    }

    pub fn text_document_document_color(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Vec<lsp::ColorInformation>>>> {
        self.capabilities.get().unwrap().color_provider.as_ref()?;
        let params = lsp::DocumentColorParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: work_done_token.clone(),
            },
            partial_result_params: helix_lsp_types::PartialResultParams {
                partial_result_token: work_done_token,
            },
        };

        Some(self.call::<lsp::request::DocumentColor>(params))
    }

    pub fn text_document_hover(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::Hover>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support hover.
        match capabilities.hover_provider {
            Some(
                lsp::HoverProviderCapability::Simple(true)
                | lsp::HoverProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            // lsp::SignatureHelpContext
        };

        Some(self.call::<lsp::request::HoverRequest>(params))
    }

    // formatting

    pub fn text_document_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TextEdit>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support formatting.
        match capabilities.document_formatting_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        };

        let options = self.get_merged_formatting_options(options);

        let params = lsp::DocumentFormattingParams {
            text_document,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::Formatting>(params))
    }

    pub fn text_document_range_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::TextEdit>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support range formatting.
        match capabilities.document_range_formatting_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        };

        let options = self.get_merged_formatting_options(options);

        let params = lsp::DocumentRangeFormattingParams {
            text_document,
            range,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        Some(self.call::<lsp::request::RangeFormatting>(params))
    }

    pub fn text_document_diagnostic(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        previous_result_id: Option<String>,
    ) -> Option<impl Future<Output = Result<lsp::DocumentDiagnosticReportResult>>> {
        let capabilities = self.capabilities();

        // Return early if the server does not support pull diagnostic.
        let identifier = match capabilities.diagnostic_provider.as_ref()? {
            lsp::DiagnosticServerCapabilities::Options(cap) => cap.identifier.clone(),
            lsp::DiagnosticServerCapabilities::RegistrationOptions(cap) => {
                cap.diagnostic_options.identifier.clone()
            }
        };

        let params = lsp::DocumentDiagnosticParams {
            text_document,
            identifier,
            previous_result_id,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::DocumentDiagnosticRequest>(params))
    }

    pub fn text_document_document_highlight(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::DocumentHighlight>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support document highlight.
        match capabilities.document_highlight_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::DocumentHighlightParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::DocumentHighlightRequest>(params))
    }

    fn goto_request<
        T: lsp::request::Request<
            Params = lsp::GotoDefinitionParams,
            Result = Option<lsp::GotoDefinitionResponse>,
        >,
    >(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<T::Result>> {
        let params = lsp::GotoDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        self.call::<T>(params)
    }

    pub fn goto_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-definition.
        match capabilities.definition_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoDefinition>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_declaration(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-declaration.
        match capabilities.declaration_provider {
            Some(
                lsp::DeclarationCapability::Simple(true)
                | lsp::DeclarationCapability::RegistrationOptions(_)
                | lsp::DeclarationCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoDeclaration>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_type_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-type-definition.
        match capabilities.type_definition_provider {
            Some(
                lsp::TypeDefinitionProviderCapability::Simple(true)
                | lsp::TypeDefinitionProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoTypeDefinition>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_implementation(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<lsp::GotoDefinitionResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-definition.
        match capabilities.implementation_provider {
            Some(
                lsp::ImplementationProviderCapability::Simple(true)
                | lsp::ImplementationProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        Some(self.goto_request::<lsp::request::GotoImplementation>(
            text_document,
            position,
            work_done_token,
        ))
    }

    pub fn goto_reference(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        include_declaration: bool,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::Location>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support goto-reference.
        match capabilities.references_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        Some(self.call::<lsp::request::References>(params))
    }

    pub fn document_symbols(
        &self,
        text_document: lsp::TextDocumentIdentifier,
    ) -> Option<impl Future<Output = Result<Option<lsp::DocumentSymbolResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support document symbols.
        match capabilities.document_symbol_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::DocumentSymbolParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::DocumentSymbolRequest>(params))
    }

    pub fn prepare_rename(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Option<impl Future<Output = Result<Option<lsp::PrepareRenameResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        match capabilities.rename_provider {
            Some(lsp::OneOf::Right(lsp::RenameOptions {
                prepare_provider: Some(true),
                ..
            })) => (),
            _ => return None,
        }

        let params = lsp::TextDocumentPositionParams {
            text_document,
            position,
        };

        Some(self.call::<lsp::request::PrepareRenameRequest>(params))
    }

    // empty string to get all symbols
    pub fn workspace_symbols(
        &self,
        query: String,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceSymbolResponse>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support workspace symbols.
        match capabilities.workspace_symbol_provider {
            Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_)) => (),
            _ => return None,
        }

        let params = lsp::WorkspaceSymbolParams {
            query,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::WorkspaceSymbolRequest>(params))
    }

    pub fn code_actions(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        context: lsp::CodeActionContext,
    ) -> Option<impl Future<Output = Result<Option<Vec<lsp::CodeActionOrCommand>>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the server does not support code actions.
        match capabilities.code_action_provider {
            Some(
                lsp::CodeActionProviderCapability::Simple(true)
                | lsp::CodeActionProviderCapability::Options(_),
            ) => (),
            _ => return None,
        }

        let params = lsp::CodeActionParams {
            text_document,
            range,
            context,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        Some(self.call::<lsp::request::CodeActionRequest>(params))
    }

    pub fn rename_symbol(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        new_name: String,
    ) -> Option<impl Future<Output = Result<Option<lsp::WorkspaceEdit>>>> {
        if !self.supports_feature(LanguageServerFeature::RenameSymbol) {
            return None;
        }

        let params = lsp::RenameParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            new_name,
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        Some(self.call::<lsp::request::Rename>(params))
    }

    pub fn command(
        &self,
        command: lsp::Command,
    ) -> Option<impl Future<Output = Result<Option<Value>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // Return early if the language server does not support executing commands.
        capabilities.execute_command_provider.as_ref()?;

        let params = lsp::ExecuteCommandParams {
            command: command.command,
            arguments: command.arguments.unwrap_or_default(),
            work_done_progress_params: lsp::WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        Some(self.call::<lsp::request::ExecuteCommand>(params))
    }

    pub fn did_change_watched_files(&self, changes: Vec<lsp::FileEvent>) {
        self.notify::<lsp::notification::DidChangeWatchedFiles>(lsp::DidChangeWatchedFilesParams {
            changes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_locale_uses_lsp_locale_shape() {
        assert_eq!(normalize_locale("en_US.UTF-8"), Some("en-US".to_string()));
        assert_eq!(normalize_locale("C"), None);
    }

    #[test]
    fn resolved_launch_prefix_and_default_arguments_are_preserved() {
        let prefix = vec!["run".to_string()];
        let defaults = vec!["--stdio".to_string()];
        let configured = vec!["--pipe".to_string()];

        assert_eq!(
            launch_arguments(&prefix, &defaults, &[]).collect::<Vec<_>>(),
            ["run", "--stdio"]
        );
        assert_eq!(
            launch_arguments(&prefix, &defaults, &configured).collect::<Vec<_>>(),
            ["run", "--stdio", "--pipe"]
        );
    }

    #[test]
    fn broken_managed_server_command_does_not_fall_back_to_path() {
        let command = "helix-lsp-test-broken-managed-command";
        let package = helix_loader::ActivePackage::new("lsp", "test", "1");
        let asset = helix_loader::RuntimeAsset::from_spec(
            package,
            helix_loader::RuntimeAssetSpec::command(
                command,
                std::env::temp_dir().join(format!(
                    "helix-lsp-missing-managed-command-{}",
                    std::process::id()
                )),
            ),
        );
        let runtime_assets =
            helix_loader::RuntimeAssets::from_snapshot(helix_loader::RuntimeSnapshot {
                generation: 1,
                assets: vec![asset],
            });

        assert!(matches!(
            runtime_assets.resolve_command(command),
            Err(helix_loader::RuntimeAssetsError::BrokenManaged { .. })
        ));
    }

    #[test]
    fn workspace_folders_publish_coherent_deduplicated_snapshots() {
        let first = lsp::Url::parse("file:///workspace/first").unwrap();
        let second = lsp::Url::parse("file:///workspace/second").unwrap();
        let folders = WorkspaceFolders::new(vec![workspace_for_uri(first)]);
        let old_snapshot = folders.snapshot();

        assert!(folders.insert(workspace_for_uri(second.clone())));
        assert!(!folders.insert(workspace_for_uri(second)));

        assert_eq!(old_snapshot.len(), 1);
        assert_eq!(folders.snapshot().len(), 2);
    }

    #[test]
    fn semantic_tokens_missing_result_id_uses_full_fallback() {
        assert_eq!(
            Client::semantic_tokens_delta_previous_result_id(true, Some("1")),
            Some("1".to_string())
        );
        assert_eq!(
            Client::semantic_tokens_delta_previous_result_id(true, None),
            None
        );
        assert_eq!(
            Client::semantic_tokens_delta_previous_result_id(false, Some("1")),
            None
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn force_kill_reaps_unresponsive_server_process() {
        let args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 60".to_string(),
        ];
        let launch = helix_loader::ResolvedLaunch {
            generation: 0,
            program: "powershell.exe".into(),
            prefix_args: Vec::new(),
            default_args: Vec::new(),
            env: Default::default(),
            origin: helix_loader::Origin::Explicit,
        };
        let identity = ClientLaunchIdentity {
            program: launch.program.clone(),
            resolved_args: args.clone(),
            resolved_environment: launch.env.clone(),
            origin: launch.origin.clone(),
            configured_environment: Default::default(),
            config: None,
            timeout: 60,
            enable_snippets: false,
        };
        let (client, _rx) = Client::start_with_launch(
            launch,
            &args,
            None,
            std::iter::empty::<(&str, &str)>(),
            std::env::current_dir().unwrap(),
            None,
            Default::default(),
            "sleeping-test-server".to_string(),
            identity,
            60,
        )
        .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), async {
                let _ = client.force_shutdown().await;
                client.wait().await
            })
            .await
            .is_err()
        );

        client.force_kill().await.unwrap();
        assert!(client.process.lock().await.try_wait().unwrap().is_some());
    }
}
