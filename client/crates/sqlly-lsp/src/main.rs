//! Binary entry point for `sqlly-lsp`. Builds the tokio runtime + shared
//! [`sqlly_services::Services`], then drives the [`LspServer`] message loop
//! over stdin/stdout until the client disconnects (clean EOF).

use std::io::{self, BufReader, BufWriter};

use sqlly_services::Services;
use tokio::runtime::Handle;
use tracing_subscriber::EnvFilter;

use sqlly_lsp::LspServer;

fn main() -> io::Result<()> {
    // Install the global panic hook first — before any work that could panic.
    sqlly_core::install_panic_hook();

    // Logging goes to stderr only (stdout is the LSP wire — must stay clean).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init()
        .ok();

    // A multi-thread tokio runtime backs the async service calls (validate,
    // search, metadata load). The read loop does blocking reads on stdin, but
    // dispatches I/O-bound requests onto the runtime so a slow query never
    // stalls the next request; replies are serialized through the server's
    // writer thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    let handle: Handle = rt.handle().clone();
    let services = Services::new();

    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    // The writer is moved onto the server's writer thread, so it must be
    // `Send + 'static`: use the owned `Stdout` handle (not a `StdoutLock`,
    // which is `!Send`). `Stdout` is itself synchronized, and the LSP server
    // funnels every write through a single thread, so there is no interleaving.
    let writer = BufWriter::new(io::stdout());

    let mut server = LspServer::new(services, handle, writer);
    server.run(&mut { reader })?;
    Ok(())
}
