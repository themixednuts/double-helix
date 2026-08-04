use crate::{ErrorCode, FileTransaction, FileTransactionReceipt, RemoteError, TransactionId};
use helix_workspace::{
    FileTransactionError, FileTransactionErrorKind, FileTransactionId, FileTransactionStore,
};
use std::path::Path;

pub(crate) struct TransactionStore {
    transactions: FileTransactionStore,
}

impl TransactionStore {
    pub(crate) fn open(root: &Path, session: crate::SessionId) -> Result<Self, RemoteError> {
        Ok(Self {
            transactions: FileTransactionStore::open_persistent(root, session.0)
                .map_err(remote_error)?,
        })
    }

    pub(crate) fn next_id(&self) -> Result<crate::TransactionId, RemoteError> {
        self.transactions
            .next_id()
            .map(|id| crate::TransactionId(id.0))
            .map_err(remote_error)
    }

    pub(crate) fn apply(
        &mut self,
        id: TransactionId,
        transaction: FileTransaction,
    ) -> Result<FileTransactionReceipt, RemoteError> {
        let receipt = self
            .transactions
            .apply(FileTransactionId(id.0), transaction)
            .map_err(remote_error)?;
        Ok(FileTransactionReceipt {
            transaction: id,
            changes: receipt.changes,
        })
    }

    pub(crate) fn undo(&mut self, id: TransactionId) -> Result<(), RemoteError> {
        self.transactions
            .undo(FileTransactionId(id.0))
            .map_err(remote_error)
    }

    pub(crate) fn clear(&mut self) {
        self.transactions.clear();
    }
}

fn remote_error(error: FileTransactionError) -> RemoteError {
    let code = match error.kind() {
        FileTransactionErrorKind::NotFound => ErrorCode::NotFound,
        FileTransactionErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        FileTransactionErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        FileTransactionErrorKind::WorkspaceOutsideRoot => ErrorCode::WorkspaceOutsideRoot,
        FileTransactionErrorKind::InvalidPath => ErrorCode::InvalidPath,
        FileTransactionErrorKind::InvalidRequest => ErrorCode::InvalidRequest,
        FileTransactionErrorKind::ResourceExhausted => ErrorCode::ResourceExhausted,
        FileTransactionErrorKind::Io => ErrorCode::Io,
    };
    RemoteError {
        code,
        message: error.to_string(),
        path: error.path().cloned(),
        retryable: error.is_retryable(),
    }
}
