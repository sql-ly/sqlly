//! The two runtime primitives the remote client needs, on both targets.
//!
//! Natively they are tokio's (the app and the CLI run a tokio runtime). In
//! the browser there is no tokio runtime: tasks go to the page's microtask
//! queue and deadlines are JS timers. Everything else the client uses from
//! tokio (`sync`, `select!`) is runtime-free and identical on both.

use std::future::Future;
use std::time::Duration;

/// Fire-and-forget a task that runs to completion on its own.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(future);
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(future);
}

/// `Err(())` when `duration` elapses before `future` completes.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, ()> {
    tokio::time::timeout(duration, future).await.map_err(|_| ())
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, ()> {
    // The JS timer future is `!Send`; the service ports need `Send` futures,
    // so the timer runs in its own local task and only a `Send` oneshot is
    // held across the await here.
    let millis = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    let (fired_tx, fired_rx) = tokio::sync::oneshot::channel::<()>();
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(millis).await;
        let _ = fired_tx.send(());
    });
    tokio::pin!(future);
    tokio::select! {
        biased;
        out = &mut future => Ok(out),
        _ = fired_rx => Err(()),
    }
}
