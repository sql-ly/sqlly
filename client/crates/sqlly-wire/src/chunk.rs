//! Message chunking and endpoint-only reassembly.
//!
//! [`split_into_chunks`] slices an opaque plaintext (already the size the caller
//! wants to seal per Noise message) into body-sized pieces. [`ChunkAssembler`]
//! runs only at endpoints — never on the relay, which forwards chunks without
//! reassembling — and rebuilds the original message while defending against
//! out-of-order delivery, duplicates, unbounded memory, and stalled transfers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

use crate::error::{AssembleError, ChunkError, TimedOutMessage};

/// Absolute ceiling on chunk count, independent of assembler config. Bounds the
/// index vector any single message can force the assembler to allocate.
pub const MAX_CHUNK_COUNT: u32 = 1 << 20;

/// Split `plaintext` into chunks no larger than `max_body` bytes each.
///
/// * An empty `plaintext` yields exactly one empty chunk, so every logical
///   message — including a zero-length one — is represented by at least one
///   chunk that the assembler can complete.
/// * A `plaintext` whose length is an exact multiple of `max_body` yields
///   exactly `len / max_body` full chunks (no trailing empty chunk).
///
/// Returns [`ChunkError::ZeroMaxBody`] if `max_body == 0`, and
/// [`ChunkError::TooManyChunks`] if the split would exceed [`MAX_CHUNK_COUNT`].
pub fn split_into_chunks(plaintext: &[u8], max_body: usize) -> Result<Vec<Bytes>, ChunkError> {
    if max_body == 0 {
        return Err(ChunkError::ZeroMaxBody);
    }

    if plaintext.is_empty() {
        return Ok(vec![Bytes::new()]);
    }

    // Ceiling division without overflow.
    let needed = plaintext.len().div_ceil(max_body);
    if needed as u64 > u64::from(MAX_CHUNK_COUNT) {
        return Err(ChunkError::TooManyChunks {
            needed: needed as u64,
            max: MAX_CHUNK_COUNT,
        });
    }

    let owned = Bytes::copy_from_slice(plaintext);
    let mut chunks = Vec::with_capacity(needed);
    let mut offset = 0;
    while offset < owned.len() {
        let end = (offset + max_body).min(owned.len());
        chunks.push(owned.slice(offset..end));
        offset = end;
    }
    Ok(chunks)
}

/// Configuration bounds for a [`ChunkAssembler`].
#[derive(Debug, Clone, Copy)]
pub struct AssemblerConfig {
    /// Maximum reconstructed size (sum of all chunk bodies) for one message.
    pub max_message_size: usize,
    /// Maximum number of messages being reassembled concurrently.
    pub max_in_flight: usize,
    /// Maximum `chunk_count` any message may declare. Also clamped to
    /// [`MAX_CHUNK_COUNT`].
    pub max_chunk_count: u32,
    /// A partially reassembled message is dropped if this long elapses since
    /// its most recent chunk.
    pub timeout: Duration,
}

impl Default for AssemblerConfig {
    fn default() -> Self {
        Self {
            max_message_size: MAX_MESSAGE_SIZE_DEFAULT,
            max_in_flight: 256,
            max_chunk_count: MAX_CHUNK_COUNT,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Default reconstructed-message ceiling (64 MiB).
pub const MAX_MESSAGE_SIZE_DEFAULT: usize = 64 * 1024 * 1024;

/// One partially reassembled message.
struct Partial {
    chunk_count: u32,
    received: u32,
    accumulated: usize,
    slots: Vec<Option<Bytes>>,
    last_activity: Instant,
}

/// Reassembles chunked messages at an endpoint.
///
/// Feed each arriving chunk to [`push`](ChunkAssembler::push); when the final
/// missing chunk of a message arrives, `push` returns the reconstructed
/// [`Bytes`]. Call [`expire`](ChunkAssembler::expire) periodically to drop and
/// report messages that stalled.
pub struct ChunkAssembler {
    cfg: AssemblerConfig,
    messages: HashMap<u64, Partial>,
}

impl ChunkAssembler {
    /// Create an assembler with the given bounds.
    #[must_use]
    pub fn new(cfg: AssemblerConfig) -> Self {
        Self {
            cfg,
            messages: HashMap::new(),
        }
    }

    /// Create an assembler with [`AssemblerConfig::default`] bounds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(AssemblerConfig::default())
    }

    /// Number of messages currently being reassembled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.messages.len()
    }

    /// The effective maximum declared `chunk_count`, honoring both the config
    /// and the absolute [`MAX_CHUNK_COUNT`] ceiling.
    fn effective_max_chunk_count(&self) -> u32 {
        self.cfg.max_chunk_count.min(MAX_CHUNK_COUNT)
    }

    /// Push one chunk into the assembler.
    ///
    /// `now` is supplied by the caller (rather than read from the clock
    /// internally) so timeouts are deterministically testable. On the chunk
    /// that completes a message, the reconstructed body is returned as
    /// `Ok(Some(_))` and the message is removed. Otherwise `Ok(None)`.
    pub fn push(
        &mut self,
        message_id: u64,
        chunk_index: u32,
        chunk_count: u32,
        payload: Bytes,
        now: Instant,
    ) -> Result<Option<Bytes>, AssembleError> {
        // Validate the chunk's self-describing fields first.
        if chunk_count == 0 {
            return Err(AssembleError::ZeroChunkCount { message_id });
        }
        let max_count = self.effective_max_chunk_count();
        if chunk_count > max_count {
            return Err(AssembleError::ChunkCountTooLarge {
                message_id,
                count: chunk_count,
                max: max_count,
            });
        }
        if chunk_index >= chunk_count {
            return Err(AssembleError::IndexOutOfRange {
                message_id,
                index: chunk_index,
                count: chunk_count,
            });
        }

        // Drop stalled messages so they don't hold in-flight slots hostage.
        self.drop_expired(now);

        let is_new = !self.messages.contains_key(&message_id);
        if is_new && self.messages.len() >= self.cfg.max_in_flight {
            return Err(AssembleError::TooManyInFlight {
                max: self.cfg.max_in_flight,
            });
        }

        // Single-chunk fast path avoids retaining state for one-shot messages.
        if is_new && chunk_count == 1 {
            if payload.len() > self.cfg.max_message_size {
                return Err(AssembleError::MessageTooLarge {
                    message_id,
                    max: self.cfg.max_message_size,
                });
            }
            return Ok(Some(payload));
        }

        let entry = self.messages.entry(message_id).or_insert_with(|| Partial {
            chunk_count,
            received: 0,
            accumulated: 0,
            slots: vec![None; chunk_count as usize],
            last_activity: now,
        });

        if entry.chunk_count != chunk_count {
            return Err(AssembleError::InconsistentChunkCount {
                message_id,
                existing: entry.chunk_count,
                got: chunk_count,
            });
        }

        let slot = &mut entry.slots[chunk_index as usize];
        if slot.is_some() {
            return Err(AssembleError::DuplicateChunk {
                message_id,
                index: chunk_index,
            });
        }

        // Enforce the reconstructed-size bound incrementally.
        let projected = entry.accumulated.saturating_add(payload.len());
        if projected > self.cfg.max_message_size {
            // Drop the whole message: it can never legally complete.
            self.messages.remove(&message_id);
            return Err(AssembleError::MessageTooLarge {
                message_id,
                max: self.cfg.max_message_size,
            });
        }

        entry.accumulated = projected;
        entry.received += 1;
        entry.last_activity = now;
        *slot = Some(payload);

        if entry.received == entry.chunk_count {
            let done = self.messages.remove(&message_id).expect("just inserted");
            let mut out = BytesMut::with_capacity(done.accumulated);
            for slot in done.slots {
                out.extend_from_slice(&slot.expect("all slots filled when complete"));
            }
            return Ok(Some(out.freeze()));
        }

        Ok(None)
    }

    /// Drop and return every message that has not received a chunk within the
    /// configured timeout, measured against `now`. Callers use the returned
    /// list to surface a missing-chunk / timeout error to the peer.
    pub fn expire(&mut self, now: Instant) -> Vec<TimedOutMessage> {
        let timeout = self.cfg.timeout;
        let mut timed_out = Vec::new();
        self.messages.retain(|&message_id, partial| {
            let stalled = now
                .checked_duration_since(partial.last_activity)
                .is_some_and(|elapsed| elapsed > timeout);
            if stalled {
                timed_out.push(TimedOutMessage {
                    message_id,
                    received: partial.received,
                    expected: partial.chunk_count,
                });
                false
            } else {
                true
            }
        });
        timed_out
    }

    /// Drop expired messages without reporting them (internal housekeeping).
    fn drop_expired(&mut self, now: Instant) {
        let timeout = self.cfg.timeout;
        self.messages.retain(|_, partial| {
            now.checked_duration_since(partial.last_activity)
                .is_none_or(|elapsed| elapsed <= timeout)
        });
    }
}
