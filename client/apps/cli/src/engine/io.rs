use super::*;

/// Blocking stdin reader: pulls length-prefixed request frames off the real
/// `std::io::stdin()` and forwards each raw payload to the async engine loop.
/// Exits when stdin closes or a frame is malformed/oversized.
pub(crate) fn stdin_reader(req_tx: mpsc::Sender<Vec<u8>>) {
    use std::io::Read;
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut len_buf = [0u8; 4];
        if stdin.read_exact(&mut len_buf).is_err() {
            return; // EOF or error: stop reading.
        }
        let len = u32::from_le_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return;
        }
        let mut payload = vec![0u8; len as usize];
        if stdin.read_exact(&mut payload).is_err() {
            return;
        }
        if req_tx.blocking_send(payload).is_err() {
            return; // Engine loop has gone away.
        }
    }
}

#[cfg(test)]
pub(crate) async fn read_frame<R>(input: &mut R) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read engine frame length"),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("engine frame too large: {len} bytes");
    }
    let mut payload = vec![0u8; len as usize];
    input
        .read_exact(&mut payload)
        .await
        .context("read engine frame payload")?;
    Ok(Some(payload))
}

/// Synchronous writer running on the blocking thread pool. Pulls frames off
/// the tokio mpsc channel via `blocking_recv` and writes them to the real
/// `std::io::stdout()` with an explicit flush per frame. This avoids
/// `tokio::io::stdout()`'s buffering, which could strand isolated/trailing
/// frames in its internal buffer (see `run_with_io`).
pub(crate) fn write_frames_blocking(mut frames: mpsc::Receiver<EngineFrame>) -> Result<()> {
    use std::io::Write;

    while let Some(frame) = frames.blocking_recv() {
        let bytes = match encode_frame(&frame) {
            Ok(bytes) => bytes,
            Err(err) => {
                if let Some(id) = frame_request_id(&frame) {
                    let fallback = EngineFrame::RequestFailed {
                        id: id.to_string(),
                        error: EngineError::protocol(format!("failed to encode response: {err}")),
                    };
                    if let Ok(fb_bytes) = encode_frame(&fallback) {
                        let stdout = std::io::stdout();
                        let mut handle = stdout.lock();
                        let _ = handle.write_all(&fb_bytes);
                        let _ = handle.flush();
                        continue;
                    }
                }
                return Err(err);
            }
        };
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(&bytes).context("write engine frame")?;
        handle.flush().context("flush engine frame")?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn write_frame<W>(output: &mut W, frame: &EngineFrame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = encode_frame(frame)?;
    output
        .write_all(&bytes)
        .await
        .context("write engine frame")?;
    output.flush().await.context("flush engine frame")?;
    Ok(())
}
