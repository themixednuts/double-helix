use std::sync::OnceLock;

pub struct RuntimeTest {
    runtime: tokio::runtime::Runtime,
    local: tokio::task::LocalSet,
}

pub fn runtime() -> crate::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    crate::Runtime::new(
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
            })
            .handle()
            .clone(),
    )
}

impl RuntimeTest {
    pub fn new() -> Self {
        Self::new_with_time(false)
    }

    pub fn new_paused() -> Self {
        Self::new_with_time(true)
    }

    fn new_with_time(start_paused: bool) -> Self {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        if start_paused {
            builder.start_paused(true);
        }
        let runtime = builder.build().expect("runtime");
        Self {
            runtime,
            local: tokio::task::LocalSet::new(),
        }
    }

    pub fn runtime(&self) -> crate::Runtime {
        crate::Runtime::new(self.runtime.handle().clone())
    }

    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(self.local.run_until(future))
    }

    pub fn advance(&self, duration: std::time::Duration) {
        self.runtime.block_on(async move {
            tokio::time::advance(duration).await;
            tokio::task::yield_now().await;
        });
    }
}

impl Default for RuntimeTest {
    fn default() -> Self {
        Self::new()
    }
}
