//! Remote row editing: engine-backed. Every operation travels to the
//! bridge-side engine as a `row_edit` request (op name + the service's own
//! request type as JSON) and executes through the SAME
//! `DefaultRowEditingService` the app uses in-process — identical provenance
//! discovery (`sp_describe_first_result_set`), primary-key guards, optimistic
//! concurrency, and transactional single-row statements. The relay never sees
//! row data in the clear (Noise end-to-end encryption), and the outcome types
//! round-trip as JSON so direct and remote editing share one vocabulary.
//!
//! [`UnavailableRowEditingService`] remains for bundles with no established
//! session (`unavailable_services`).

use crate::remote::engine::{RemoteEngine, decode_report_json, engine_connection_json};
use crate::row_editing::{
    BatchOutcome, BatchRequest, ColumnExpression, DeleteRowOutcome, DeleteRowRequest,
    FkLabelRequest, FkLabelResult, FkSearchRequest, FkSearchResult, InsertRowOutcome,
    InsertRowRequest, RelatedRowsRequest, RelatedRowsResult, RowEditOpenOutcome,
    RowEditOpenRequest, RowEditingService, StagedOp, TableTarget, UpdateRowOutcome,
    UpdateRowRequest,
};
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlly_core::{CellValue, ConnectionProfile, StudioError, StudioResult};
use sqlly_remote::engine_protocol::{
    FRAME_TAG_ROW_EDIT_REPORT, FramedTransport, REQUEST_TAG_ROW_EDIT,
};

/// Engine-backed [`RowEditingService`] for relay sessions.
pub struct RemoteRowEditingService<T> {
    engine: RemoteEngine<T>,
}

impl<T: FramedTransport + 'static> RemoteRowEditingService<T> {
    pub fn new(engine: RemoteEngine<T>) -> Self {
        Self { engine }
    }

    /// Run one row-edit op: serialize the request, round-trip through the
    /// engine's report path, deserialize the outcome.
    async fn op<Req: Serialize, Out: DeserializeOwned>(
        &self,
        profile: &ConnectionProfile,
        op: &'static str,
        request: &Req,
    ) -> StudioResult<Out> {
        let conn = engine_connection_json(profile)?;
        let payload = serde_json::to_string(request)
            .map_err(|e| StudioError::InvalidInput(format!("encode row_edit {op} request: {e}")))?;
        let body = self
            .engine
            .report_request(REQUEST_TAG_ROW_EDIT, FRAME_TAG_ROW_EDIT_REPORT, |id| {
                sqlly_remote::engine_protocol::layouts::request_header(id)
                    .string(&conn)
                    .string(op)
                    .string(&payload)
                    .done()
            })
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Connection(format!("decode row_edit {op} outcome: {e}")))
    }

    /// Refuse, before anything is sent, a write carrying an expression the
    /// bridge-side engine has not said it understands: an older engine would
    /// fail the whole request with a baffling "unknown variant".
    fn check_expressions(&self, expressions: &[(String, ColumnExpression)]) -> StudioResult<()> {
        if self.server_expressions_supported() {
            return Ok(());
        }
        match expressions
            .iter()
            .find(|(_, expr)| expr.needs_bridge_capability())
        {
            Some((column, _)) => Err(StudioError::CapabilityNotSupportedMessage(
                sqlly_core::DisplayMessage::new(
                    "services.remote.row_editing.bridge_old_expression_column",
                    format!("{BRIDGE_TOO_OLD_FOR_SERVER_EXPRESSIONS} (column {column})"),
                )
                .with_arg("column_name", column),
            )),
            None => Ok(()),
        }
    }
}

/// Why a server-evaluated expression was refused on this relay session.
pub const BRIDGE_TOO_OLD_FOR_SERVER_EXPRESSIONS: &str = "The SQLLY bridge on this relay connection is an older version that cannot evaluate server date, server UUID, or current user; update the bridge, or use New UUID or the local-clock timestamp instead";

pub fn bridge_too_old_for_server_expressions_message() -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "services.remote.row_editing.bridge_old_expressions",
        BRIDGE_TOO_OLD_FOR_SERVER_EXPRESSIONS,
    )
}

/// Mirrors the engine's `FetchRowPayload`: `fetch_row` takes bare arguments,
/// so the wire carries them in one struct.
#[derive(Serialize)]
struct FetchRowPayload<'a> {
    target: &'a TableTarget,
    key: &'a [(String, CellValue)],
}

#[async_trait]
impl<T: FramedTransport + 'static> RowEditingService for RemoteRowEditingService<T> {
    fn server_expressions_supported(&self) -> bool {
        self.engine
            .supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS)
    }

    fn batch_commit_supported(&self) -> bool {
        self.engine
            .supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_APPLY_BATCH)
    }

    async fn open_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        self.op(profile, "open_session", &request).await
    }

    async fn open_insert_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        self.op(profile, "open_insert_session", &request).await
    }

    async fn fetch_row(
        &self,
        profile: &ConnectionProfile,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
        self.op(profile, "fetch_row", &FetchRowPayload { target, key })
            .await
    }

    async fn update_row(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
    ) -> StudioResult<UpdateRowOutcome> {
        self.check_expressions(&request.expressions)?;
        self.op(profile, "update_row", &request).await
    }

    async fn fk_search(
        &self,
        profile: &ConnectionProfile,
        request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult> {
        self.op(profile, "fk_search", &request).await
    }

    async fn fk_labels(
        &self,
        profile: &ConnectionProfile,
        request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult> {
        // Engines before `row_edit.fk_labels` reject the op outright; say so
        // plainly instead of surfacing "unknown row_edit op".
        if !self
            .engine
            .supports(sqlly_remote::engine_protocol::CAPABILITY_ROW_EDIT_FK_LABELS)
        {
            return Err(StudioError::CapabilityNotSupported(
                "foreign-key labels need a newer SQLLY bridge on this relay connection".to_string(),
            ));
        }
        self.op(profile, "fk_labels", &request).await
    }

    async fn related_rows(
        &self,
        profile: &ConnectionProfile,
        request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult> {
        self.op(profile, "related_rows", &request).await
    }

    async fn insert_row(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
    ) -> StudioResult<InsertRowOutcome> {
        self.check_expressions(&request.expressions)?;
        self.op(profile, "insert_row", &request).await
    }

    async fn delete_row(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
    ) -> StudioResult<DeleteRowOutcome> {
        self.op(profile, "delete_row", &request).await
    }

    async fn apply_batch(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
    ) -> StudioResult<BatchOutcome> {
        if !self.batch_commit_supported() {
            return Err(StudioError::CapabilityNotSupportedMessage(
                bridge_too_old_for_batch_save_message(),
            ));
        }
        for op in &request.batch.ops {
            match op {
                StagedOp::Update(update) => self.check_expressions(&update.expressions)?,
                StagedOp::Insert(insert) => self.check_expressions(&insert.expressions)?,
                StagedOp::Delete { .. } => {}
            }
        }
        self.op(profile, "apply_batch", &request).await
    }
}

/// Why a staged grid save was refused on this relay session.
pub const BRIDGE_TOO_OLD_FOR_BATCH_SAVE: &str = "The SQLLY bridge on this relay connection is an older version that cannot save staged grid changes; update the bridge, or edit rows one at a time in the row editor";

pub fn bridge_too_old_for_batch_save_message() -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "services.remote.row_editing.bridge_old_batch_save",
        BRIDGE_TOO_OLD_FOR_BATCH_SAVE,
    )
}

/// Row editing is unavailable over the relay transport.
pub struct UnavailableRowEditingService;

fn unsupported<T>() -> StudioResult<T> {
    Err(StudioError::CapabilityNotSupportedMessage(
        sqlly_core::DisplayMessage::new(
            "services.remote.row_editing.unavailable",
            "row editing is not available over a relay connection",
        ),
    ))
}

#[async_trait]
impl RowEditingService for UnavailableRowEditingService {
    fn server_expressions_supported(&self) -> bool {
        false
    }

    fn batch_commit_supported(&self) -> bool {
        false
    }

    async fn open_session(
        &self,
        _profile: &ConnectionProfile,
        _request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        unsupported()
    }

    async fn open_insert_session(
        &self,
        _profile: &ConnectionProfile,
        _request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        unsupported()
    }

    async fn fetch_row(
        &self,
        _profile: &ConnectionProfile,
        _target: &TableTarget,
        _key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
        unsupported()
    }

    async fn update_row(
        &self,
        _profile: &ConnectionProfile,
        _request: UpdateRowRequest,
    ) -> StudioResult<UpdateRowOutcome> {
        unsupported()
    }

    async fn fk_search(
        &self,
        _profile: &ConnectionProfile,
        _request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult> {
        unsupported()
    }

    async fn fk_labels(
        &self,
        _profile: &ConnectionProfile,
        _request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult> {
        unsupported()
    }

    async fn related_rows(
        &self,
        _profile: &ConnectionProfile,
        _request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult> {
        unsupported()
    }

    async fn insert_row(
        &self,
        _profile: &ConnectionProfile,
        _request: InsertRowRequest,
    ) -> StudioResult<InsertRowOutcome> {
        unsupported()
    }

    async fn delete_row(
        &self,
        _profile: &ConnectionProfile,
        _request: DeleteRowRequest,
    ) -> StudioResult<DeleteRowOutcome> {
        unsupported()
    }

    async fn apply_batch(
        &self,
        _profile: &ConnectionProfile,
        _request: BatchRequest,
    ) -> StudioResult<BatchOutcome> {
        unsupported()
    }
}
