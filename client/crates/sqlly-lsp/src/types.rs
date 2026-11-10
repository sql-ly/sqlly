//! LSP type aliases and typed params for the subset of the Language Server
//! Protocol the `sqlly-lsp` server implements. Only the shapes we read/write
//! are defined — no full LSP type crate is pulled in, keeping the surface tiny.
//!
//! References: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/

use serde::{Deserialize, Serialize};

/// A 0-based (line, character) position, UTF-16 code units (LSP default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A half-open range [`start`, `end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A text document identifier (no version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

/// A versioned text document identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
    pub version: i32,
}

/// `textDocument/completion` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

/// `textDocument/hover` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

/// `textDocument/didOpen` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

/// A text document item (uri + languageId + version + text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: i32,
    pub text: String,
}

/// `textDocument/didChange` params. Changes may be ranged incremental
/// edits (UTF-16 positions) or full replacements; the server advertises
/// `TextDocumentSyncKind::Incremental` and applies each change in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: VersionedTextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentContentChangeEvent {
    /// The edited region, in UTF-16 positions. Absent means the event carries
    /// the full replacement text (full-sync clients, or a full refresh from an
    /// incremental one).
    #[serde(default)]
    pub range: Option<Range>,
    pub text: String,
}

/// `workspace/symbol` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSymbolParams {
    pub query: String,
}

/// LSP `initialize` params (the subset we read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    #[serde(default)]
    pub capabilities: serde_json::Value,
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default)]
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

/// `initialize` result: server capabilities + connection info.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// The subset of LSP server capabilities `sqlly-lsp` advertises.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub text_document_sync: TextDocumentSyncKind,
    pub completion_provider: CompletionProvider,
    pub hover_provider: bool,
    pub diagnostic_provider: DiagnosticOptions,
    pub workspace_symbol_provider: bool,
    pub semantic_tokens_provider: SemanticTokensOptions,
    pub definition_provider: bool,
    pub references_provider: bool,
    pub document_symbol_provider: bool,
    pub document_formatting_provider: bool,
}

/// `textDocument/didClose` params.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

/// `textDocument/didSave` params (text omitted; the server re-reads its copy).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidSaveTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

/// `textDocument/formatting` params.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentFormattingParams {
    pub text_document: TextDocumentIdentifier,
    #[serde(default)]
    pub options: FormattingOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormattingOptions {
    #[serde(default)]
    pub tab_size: Option<u32>,
    #[serde(default)]
    pub insert_spaces: Option<bool>,
}

/// A single text replacement, the formatting response unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// `textDocument/documentSymbol` params.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbolParams {
    pub text_document: TextDocumentIdentifier,
}

/// Hierarchical document symbol (flat here — SQL statements have no nesting
/// the tree needs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: i32,
    pub range: Range,
    pub selection_range: Range,
}

/// `textDocument/references` params: a position plus the declaration flag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    #[serde(default)]
    pub context: ReferenceContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceContext {
    #[serde(default)]
    pub include_declaration: bool,
}

/// LSP wire encoding is the NUMERIC TextDocumentSyncKind (1 = full,
/// 2 = incremental) — the camelCase string this previously serialized as is
/// not valid LSP and real clients ignore it (falling back to no sync).
#[derive(Debug, Clone, Copy)]
pub enum TextDocumentSyncKind {
    /// Full document text sent on every change.
    Full = 1,
    /// Ranged edits sent on every change (UTF-16 positions).
    Incremental = 2,
}

impl Serialize for TextDocumentSyncKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionProvider {
    pub trigger_characters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticOptions {
    pub inter_file_dependencies: bool,
    pub workspace_diagnostics: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensOptions {
    pub legend: SemanticTokensLegend,
    pub full: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensLegend {
    pub token_types: Vec<String>,
    pub token_modifiers: Vec<String>,
}

/// LSP CompletionItemKind values (subset — T-SQL object kinds map to these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionItemKind {
    Text = 1,
    Keyword = 14,
    Snippet = 15,
    Class = 7,
    Function = 3,
    Variable = 6,
    Field = 5,
    Module = 9,
    TypeParameter = 25,
    Interface = 8,
}

/// A completion item returned to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemLsp {
    pub label: String,
    pub kind: i32,
    pub insert_text: String,
}

/// A diagnostic returned to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticLsp {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: String,
}

/// LSP DiagnosticSeverity (subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

/// `textDocument/publishDiagnostics` notification params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<DiagnosticLsp>,
    /// The document version these diagnostics were computed against, when
    /// known — lets the client discard reports for versions it has replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i32>,
}

/// `textDocument/diagnostic` (pull model) request params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDiagnosticParams {
    pub text_document: TextDocumentIdentifier,
}

/// A full-document diagnostic report for the pull model. `kind` is always
/// `"full"`; `items` carries the diagnostics.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDiagnosticReport {
    pub kind: String,
    pub items: Vec<DiagnosticLsp>,
}

/// A hover result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hover {
    pub contents: MarkupContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkupContent {
    pub kind: String,
    pub value: String,
}

/// A workspace symbol (for `workspace/symbol`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
    pub name: String,
    pub kind: i32,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// `textDocument/semanticTokens/full` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokens {
    pub data: Vec<u32>,
}
