//! Spawning on the tokio runtime with each future or closure boxed first. tokio builds its task
//! code once per task type, so unboxed every spawn site would carry its own copy (~4 KB each);
//! boxed, the sites share one copy per result type (less generated code). The cost is
//! one allocation per spawn, which the window's tasks never notice.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type BoxFn<T> = Box<dyn FnOnce() -> T + Send + 'static>;

/// A runtime [`Handle`] whose `spawn` and `spawn_blocking` box the task; everything else is the
/// handle's own (through `Deref`).
#[derive(Clone, Debug)]
pub struct Rt(Handle);

impl Rt {
    pub fn new(handle: Handle) -> Self {
        Self(handle)
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.0.spawn(Box::pin(future) as BoxFuture<F::Output>)
    }

    pub fn spawn_blocking<F, R>(&self, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.0.spawn_blocking(Box::new(work) as BoxFn<R>)
    }
}

impl Deref for Rt {
    type Target = Handle;

    fn deref(&self) -> &Handle {
        &self.0
    }
}

/// [`tokio::task::spawn_blocking`] on the current runtime, boxed as [`Rt::spawn_blocking`] is.
pub fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(Box::new(work) as BoxFn<R>)
}
