//! Package manager engine for LSP servers, DAP adapters, grammars, and plugins.

pub mod config;
pub mod lock;
pub mod ops;
pub mod registry;
pub mod resolve;
pub mod spec;
pub mod store;

pub use config::{NativeInstallPolicy, PkgConfig, Policy, RegistrySource};
pub use lock::{Lock, LockedPackage, Manifest};
pub use ops::{
    release_age_label, Backend, BackendInstall, DoctorReport, LockOptions, OpEvent, Ops,
    PackageChange, PluginBackend, PluginBackendTransport, RegistryUpdate, ResolvedPackage,
    UpdatePlan,
};
pub use registry::Registry;
pub use spec::{Artifact, NativeManager, NativeSource, PackageSpec, PkgKind, Source};
pub use store::{Receipt, Store};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML at {path}: {source}")]
    TomlDe {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to write TOML: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("failed to encode or decode receipt JSON: {0}")]
    ReceiptJson(#[from] serde_json::Error),
    #[error("SQLite store error: {0}")]
    Store(#[from] helix_store::Error),
    #[error("http request failed for {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("json response from {url} was invalid: {source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("zip archive {path} is invalid: {source}")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("unsupported archive format: {0}")]
    UnsupportedArchive(String),
    #[error("package not found: {0}")]
    NotFound(String),
    #[error("no artifact for {name} on {os}/{arch}")]
    NoArtifact {
        name: String,
        os: String,
        arch: String,
    },
    #[error("invalid package {name}: {message}")]
    InvalidPackage { name: String, message: String },
    #[error("sha256 mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("system command not found on PATH: {0}")]
    SystemMissing(String),
    #[error("command failed: {program} {args}\nstdout: {stdout}\nstderr: {stderr}")]
    CommandFailed {
        program: String,
        args: String,
        stdout: String,
        stderr: String,
    },
    #[error("policy violation ({key}): {message}")]
    PolicyViolation { key: &'static str, message: String },
    #[error("{0}")]
    Message(String),
}

pub(crate) fn io(path: impl std::fmt::Display, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_string(),
        source,
    }
}
