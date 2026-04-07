use std::future::IntoFuture;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::task::Local;

#[derive(Clone)]
pub struct Ui {
    thread: std::thread::ThreadId,
    _not_send: PhantomData<Rc<()>>,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field("is_current", &self.is_current())
            .finish()
    }
}

impl Ui {
    pub(crate) fn new(thread: std::thread::ThreadId) -> Self {
        Self {
            thread,
            _not_send: PhantomData,
        }
    }

    pub fn is_current(&self) -> bool {
        crate::current_thread_id() == self.thread
    }

    pub fn spawn<F>(&self, future: F) -> Local<F::Output>
    where
        F: IntoFuture + 'static,
        F::IntoFuture: 'static,
        F::Output: 'static,
    {
        Local::new(tokio::task::spawn_local(future.into_future()))
    }
}
