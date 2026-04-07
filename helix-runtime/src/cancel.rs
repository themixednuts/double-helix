#[derive(Clone, Default)]
pub struct Token {
    inner: tokio_util::sync::CancellationToken,
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
        }
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
