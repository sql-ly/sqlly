//! Engine capability negotiation, carried in the `Ready` frame.
//!
//! The engine's first frame has always been `Ready` with a body of exactly
//! `[u32 protocol_version]`. Every decoder that predates capabilities (the Rust
//! remote demux, the CLI test harnesses, the frozen Swift client) reads that
//! `u32` and ignores whatever follows, so newer engines append an optional
//! capability list after it:
//!
//! ```text
//! [u32 protocol_version][u32 count][u32 len][utf8 capability] × count
//! ```
//!
//! This keeps [`PROTOCOL_VERSION`](crate::engine_protocol::PROTOCOL_VERSION)
//! unchanged (a bump would make old peers refuse the session outright) while
//! letting a client ask "does the engine on the other end of this relay
//! understand X?" before it sends X. A `Ready` body with nothing after the
//! version comes from an engine that predates negotiation: it advertises no
//! capabilities, and callers must treat every capability as absent.
//!
//! Capability names are dotted, lowercase strings (`area.feature`). They are
//! only ever added, never renamed: a client checks for the name it needs.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::engine_protocol::error::{ProtocolError, ProtocolResult};
use crate::engine_protocol::reader::BinaryReader;
use crate::engine_protocol::writer::BinaryWriter;

/// Row editing accepts the server-evaluated `ColumnExpression` variants added
/// after bridges first shipped (`ServerDate`, `ServerUuid`, `CurrentUser`).
/// An engine without it fails the whole `row_edit` request with "unknown
/// variant" when it sees one.
pub const CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS: &str = "row_edit.server_expressions";

/// Row editing answers the `fk_labels` op (batched FK display labels). Older
/// engines reject it as an unknown `row_edit` op.
pub const CAPABILITY_ROW_EDIT_FK_LABELS: &str = "row_edit.fk_labels";

/// Row editing answers the `apply_batch` op: a staged batch committed in one
/// transaction with the same per-statement affected-row guards as a local
/// save. Older engines reject it as an unknown `row_edit` op.
pub const CAPABILITY_ROW_EDIT_APPLY_BATCH: &str = "row_edit.apply_batch";

/// `execute_query` accepts the trailing options word (see
/// [`crate::engine_protocol::layouts::encode_execute_query_with_options`]) and
/// honors [`crate::engine_protocol::EXECUTE_QUERY_OPTION_CELL_PREVIEWS`]:
/// text and binary cells over 8 KiB are cut to previews engine-side and every
/// cut is reported in a `truncated_cells` frame. An engine without it rejects
/// the options word as trailing bytes, so clients send it only when this is
/// advertised; without the option (old clients, full-value exports) cells
/// arrive whole, as they always have.
pub const CAPABILITY_QUERY_CELL_PREVIEWS: &str = "query.cell_previews";

/// Most capabilities a `Ready` body may carry. Far above any real list; it only
/// bounds what a hostile or corrupt peer can make the client allocate.
pub const MAX_READY_CAPABILITIES: usize = 256;

/// Longest capability name accepted, in bytes.
pub const MAX_CAPABILITY_NAME_LEN: usize = 128;

/// The capability set one engine advertised. Cheap to clone (shared).
///
/// `EngineCapabilities::default()` is the empty set — what an engine that
/// predates negotiation is treated as having.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineCapabilities(Arc<BTreeSet<String>>);

impl EngineCapabilities {
    /// Build a set from capability names.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(Arc::new(names.into_iter().map(Into::into).collect()))
    }

    /// Whether the engine advertised `capability`.
    #[must_use]
    pub fn supports(&self, capability: &str) -> bool {
        self.0.contains(capability)
    }

    /// True when nothing was advertised (including a pre-negotiation engine).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The advertised names, sorted.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// A decoded `Ready` frame body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyInfo {
    /// The engine's protocol version (always present).
    pub protocol_version: u32,
    /// What the engine advertised; empty for an engine that predates
    /// negotiation.
    pub capabilities: EngineCapabilities,
}

/// Encode a `Ready` body: the legacy `[u32 protocol_version]` prefix, then the
/// capability list. Names over [`MAX_CAPABILITY_NAME_LEN`] or beyond
/// [`MAX_READY_CAPABILITIES`] would be rejected by the decoder, so they are an
/// encoding error here rather than a silent truncation.
pub fn encode_ready_body(protocol_version: u32, capabilities: &[&str]) -> ProtocolResult<Vec<u8>> {
    if capabilities.len() > MAX_READY_CAPABILITIES {
        return Err(ProtocolError::decode(format!(
            "ready: {} capabilities exceeds the {MAX_READY_CAPABILITIES} limit",
            capabilities.len()
        )));
    }
    let mut writer = BinaryWriter::new()
        .u32_le(protocol_version)
        .u32_le(capabilities.len() as u32);
    for name in capabilities {
        if name.is_empty() || name.len() > MAX_CAPABILITY_NAME_LEN {
            return Err(ProtocolError::decode(format!(
                "ready: capability name of {} bytes is outside 1..={MAX_CAPABILITY_NAME_LEN}",
                name.len()
            )));
        }
        writer = writer.string(name);
    }
    Ok(writer.done())
}

/// Decode a `Ready` body from any engine, old or new.
///
/// * `[u32]` alone (an engine that predates negotiation) → empty capabilities.
/// * `[u32][list]` → the advertised set.
///
/// A body too short for the version is an error. A capability list that is
/// present but malformed (truncated, over the limits, bad UTF-8, trailing
/// bytes) is also an error: the caller keeps the session but must treat the
/// engine as advertising nothing, never as advertising a partial list.
pub fn decode_ready_body(body: &[u8]) -> ProtocolResult<ReadyInfo> {
    let mut reader = BinaryReader::new(body);
    let protocol_version = reader.read_u32_le("protocol_version")?;
    if reader.remaining() == 0 {
        return Ok(ReadyInfo {
            protocol_version,
            capabilities: EngineCapabilities::default(),
        });
    }
    let count = reader.read_u32_le("capability_count")? as usize;
    if count > MAX_READY_CAPABILITIES {
        return Err(ProtocolError::decode(format!(
            "ready: {count} capabilities exceeds the {MAX_READY_CAPABILITIES} limit"
        )));
    }
    let mut names = BTreeSet::new();
    for _ in 0..count {
        // Check the declared length before `read_string` copies anything.
        let declared =
            BinaryReader::new(&body[reader.offset()..]).read_u32_le("capability.length")? as usize;
        if declared == 0 || declared > MAX_CAPABILITY_NAME_LEN {
            return Err(ProtocolError::decode(format!(
                "ready: capability name of {declared} bytes is outside 1..={MAX_CAPABILITY_NAME_LEN}"
            )));
        }
        names.insert(reader.read_string("capability")?);
    }
    reader.ensure_at_end()?;
    Ok(ReadyInfo {
        protocol_version,
        capabilities: EngineCapabilities(Arc::new(names)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_body_decodes_with_no_capabilities() {
        // Exactly what every engine before negotiation sends.
        let body = 2u32.to_le_bytes().to_vec();
        let info = decode_ready_body(&body).unwrap();
        assert_eq!(info.protocol_version, 2);
        assert!(info.capabilities.is_empty());
        assert!(
            !info
                .capabilities
                .supports(CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS)
        );
    }

    #[test]
    fn new_body_round_trips() {
        let body = encode_ready_body(
            2,
            &[
                CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS,
                CAPABILITY_ROW_EDIT_FK_LABELS,
            ],
        )
        .unwrap();
        let info = decode_ready_body(&body).unwrap();
        assert_eq!(info.protocol_version, 2);
        assert!(
            info.capabilities
                .supports(CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS)
        );
        assert!(info.capabilities.supports(CAPABILITY_ROW_EDIT_FK_LABELS));
        assert!(!info.capabilities.supports("row_edit.apply_batch"));
    }

    /// An OLD client reads exactly `[u32]` and ignores the rest, so the new
    /// body must begin with the legacy layout byte for byte.
    #[test]
    fn new_body_keeps_the_legacy_prefix_for_old_clients() {
        let body = encode_ready_body(7, &[CAPABILITY_ROW_EDIT_FK_LABELS]).unwrap();
        assert_eq!(&body[..4], &7u32.to_le_bytes());
        let mut legacy = BinaryReader::new(&body);
        assert_eq!(legacy.read_u32_le("protocol_version").unwrap(), 7);
    }

    #[test]
    fn empty_list_is_distinct_from_legacy_but_equally_empty() {
        let body = encode_ready_body(2, &[]).unwrap();
        assert_eq!(body.len(), 8);
        assert!(decode_ready_body(&body).unwrap().capabilities.is_empty());
    }

    #[test]
    fn truncated_or_hostile_lists_are_errors_not_partial_sets() {
        let good = encode_ready_body(2, &["a.b", "c.d"]).unwrap();
        // Chop the last name mid-string.
        assert!(decode_ready_body(&good[..good.len() - 1]).is_err());
        // Trailing garbage after a well-formed list.
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_ready_body(&trailing).is_err());
        // A count far beyond the limit must fail before allocating.
        let huge = BinaryWriter::new().u32_le(2).u32_le(u32::MAX).done();
        assert!(decode_ready_body(&huge).is_err());
        // A single name declaring gigabytes.
        let long = BinaryWriter::new()
            .u32_le(2)
            .u32_le(1)
            .u32_le(u32::MAX)
            .done();
        assert!(decode_ready_body(&long).is_err());
        // Too short even for the version.
        assert!(decode_ready_body(&[1, 0]).is_err());
    }

    #[test]
    fn encoder_refuses_what_the_decoder_would_reject() {
        assert!(encode_ready_body(2, &[""]).is_err());
        let long = "x".repeat(MAX_CAPABILITY_NAME_LEN + 1);
        assert!(encode_ready_body(2, &[long.as_str()]).is_err());
    }
}
