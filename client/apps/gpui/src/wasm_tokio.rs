//! Browser (wasm) stand-in for the tokio APIs the app uses.
//!
//! Native builds run service futures on a real tokio runtime because the
//! engine's TDS/network paths need the tokio reactor. The browser services
//! are reactor-free futures, so this shim gives the exact same call-site
//! shapes (`crate::tokio_handle().spawn(..).await`, `tokio::sync::mpsc`,
//! `tokio::sync::oneshot`, `tokio::task::spawn_blocking`) with the futures
//! polled on the browser's single-threaded executor instead.
//!
//! Modules that use `tokio::` paths import this as
//! `#[cfg(target_arch = "wasm32")] use crate::wasm_tokio as tokio;` so their
//! native code stays byte-identical.

pub mod runtime {
    /// Stand-in for `tokio::runtime::Handle`. Spawning schedules the future
    /// on the browser microtask queue via `wasm_bindgen_futures`.
    #[derive(Clone, Debug)]
    pub struct Handle;

    impl Handle {
        pub fn spawn<F>(&self, future: F) -> super::task::JoinHandle<F::Output>
        where
            F: std::future::Future + 'static,
            F::Output: 'static,
        {
            super::task::spawn(future)
        }

        pub fn spawn_blocking<F, R>(&self, f: F) -> super::task::JoinHandle<R>
        where
            F: FnOnce() -> R + 'static,
            R: 'static,
        {
            super::task::spawn_blocking(f)
        }
    }
}

pub mod task {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Mirrors `tokio::task::JoinError` far enough for the app's
    /// `join_err.to_string()` reporting.
    #[derive(Debug)]
    pub struct JoinError;

    impl std::fmt::Display for JoinError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("browser task ended before reporting a result")
        }
    }

    impl std::error::Error for JoinError {}

    /// Awaitable handle to a spawned task; resolves with the task's output
    /// (or `Err(JoinError)` if the task was dropped without finishing).
    pub struct JoinHandle<T>(futures::channel::oneshot::Receiver<T>);

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.0)
                .poll(cx)
                .map(|result| result.map_err(|_| JoinError))
        }
    }

    /// Schedules `future` on the browser microtask queue. The task runs to
    /// completion whether or not the returned handle is awaited (mirroring
    /// tokio's fire-and-forget `spawn`).
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (tx, rx) = futures::channel::oneshot::channel();
        let task = async move {
            let _ = tx.send(future.await);
        };
        #[cfg(target_feature = "atomics")]
        if !super::on_page_thread() {
            super::post_to_page_thread(Box::pin(task));
            return JoinHandle(rx);
        }
        wasm_bindgen_futures::spawn_local(task);
        JoinHandle(rx)
    }

    /// There are no worker threads in the browser build; the closure runs
    /// inline on the single thread when the task is polled.
    pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + 'static,
        R: 'static,
    {
        spawn(async move { f() })
    }
}

/// Threaded (atomics) builds run gpui background tasks on Web Workers, and a
/// worker never returns to its JS event loop, so `spawn_local` there would
/// never poll. The browser services also live on the page's thread (the
/// in-tab SQLite connection is a thread-local; PGlite and fetch are page JS
/// objects). So futures spawned off the page thread are posted here and run
/// on the page thread; their result still crosses back over the oneshot.
#[cfg(target_feature = "atomics")]
mod page_thread {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::OnceLock;

    use futures::StreamExt;
    use futures::channel::mpsc::{UnboundedSender, unbounded};

    /// SPIKE: asserts the posted future is safe to move to the page thread.
    /// Futures reaching `spawn` from a background task were created inside a
    /// `Send` gpui task, but `spawn` has no `Send` bound, so this is unchecked.
    struct PageTask(Pin<Box<dyn Future<Output = ()>>>);
    unsafe impl Send for PageTask {}

    static MAILBOX: OnceLock<UnboundedSender<PageTask>> = OnceLock::new();

    pub(super) fn on_page_thread() -> bool {
        web_sys::window().is_some()
    }

    pub(super) fn install() {
        let (tx, mut rx) = unbounded::<PageTask>();
        if MAILBOX.set(tx).is_err() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(task) = rx.next().await {
                wasm_bindgen_futures::spawn_local(task.0);
            }
        });
    }

    pub(super) fn post(task: Pin<Box<dyn Future<Output = ()>>>) {
        let mailbox = MAILBOX
            .get()
            .expect("install_browser_task_mailbox runs before any background task");
        let _ = mailbox.unbounded_send(PageTask(task));
    }
}

#[cfg(target_feature = "atomics")]
use page_thread::{on_page_thread, post as post_to_page_thread};

pub(crate) fn install_main_thread_mailbox() {
    #[cfg(target_feature = "atomics")]
    page_thread::install();
}

pub mod sync {
    pub mod mpsc {
        /// tokio-shaped bounded channel over an unbounded futures channel.
        /// The capacity argument is accepted and ignored — on the
        /// single-threaded browser executor an unbounded buffer cannot dead-
        /// lock, and never dropping events is the safer behavior for the
        /// query-event stream.
        pub fn channel<T>(_capacity: usize) -> (Sender<T>, Receiver<T>) {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            (Sender(tx), Receiver(rx))
        }

        pub struct Sender<T>(futures::channel::mpsc::UnboundedSender<T>);

        impl<T> Clone for Sender<T> {
            fn clone(&self) -> Self {
                Self(self.0.clone())
            }
        }

        #[derive(Debug)]
        pub struct SendError<T>(pub T);

        impl<T> Sender<T> {
            pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
                self.0
                    .unbounded_send(value)
                    .map_err(|e| SendError(e.into_inner()))
            }
        }

        pub struct Receiver<T>(futures::channel::mpsc::UnboundedReceiver<T>);

        impl<T> Receiver<T> {
            pub async fn recv(&mut self) -> Option<T> {
                use futures::StreamExt;
                self.0.next().await
            }
        }
    }

    /// `futures`' oneshot has the same `send(self, T) -> Result<(), T>` /
    /// awaitable-receiver surface the app relies on.
    pub mod oneshot {
        pub use futures::channel::oneshot::{Canceled, Receiver, Sender, channel};
    }
}
