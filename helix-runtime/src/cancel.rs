use std::sync::Arc;

#[derive(Clone)]
pub struct Token {
    inner: tokio_util::sync::CancellationToken,
    identity: Arc<()>,
}

impl Default for Token {
    fn default() -> Self {
        Self {
            inner: tokio_util::sync::CancellationToken::new(),
            identity: Arc::new(()),
        }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("canceled", &self.is_canceled())
            .finish()
    }
}

impl Token {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn child(&self) -> Self {
        Self {
            inner: self.inner.child_token(),
            identity: Arc::new(()),
        }
    }

    /// Returns whether both handles refer to the same cancellation request.
    pub fn same_token(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.is_cancelled()
    }

    pub async fn canceled(&self) {
        self.inner.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn clones_preserve_identity_but_children_do_not() {
        let token = Token::new();
        let clone = token.clone();
        let child = token.child();

        assert!(token.same_token(&clone));
        assert!(!token.same_token(&child));
        token.cancel();
        assert!(clone.is_canceled());
        assert!(child.is_canceled());
    }
}
