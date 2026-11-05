//! Spawning with each future or closure boxed first. tokio builds its task code once per task
//! type, so unboxed every spawn site carries its own copy (~4 KB); boxed, the sites share one
//! copy per result type (S8: exe size). The cost is one allocation per spawn.

use std::future::Future;
use std::pin::Pin;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// [`Handle::spawn`], boxed.
pub(crate) fn spawn_on<F>(rt: &Handle, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    rt.spawn(Box::pin(future) as Pin<Box<dyn Future<Output = F::Output> + Send>>)
}

/// [`tokio::task::spawn_blocking`], boxed.
pub(crate) fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(Box::new(work) as Box<dyn FnOnce() -> R + Send>)
}
