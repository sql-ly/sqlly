//! Language features: the pure-CPU request handlers (completion, hover,
//! definition/references, semantic tokens, document symbols, formatting), the
//! spawned `workspace/symbol` search, and the reusable building blocks behind
//! them.
//!
//! The free functions here extract the pure logic behind each `handle_*`
//! request handler so it can be reused without a live `LspServer` (notably by
//! the GPUI IntelliSense debug screen, which re-runs the exact same `Services`
//! calls the language server makes and renders the results in LSP wire
//! format). The `LspServer` handlers call these, guaranteeing the debug screen
//! and the real server can never drift apart.

use sqlly_intel::intellisense::CompletionItem;
use sqlly_services::ProcedureInfo;

#[cfg(not(target_arch = "wasm32"))]
use crate::jsonrpc::{Id, Message, error_code};
use crate::types::*;

#[cfg(not(target_arch = "wasm32"))]
use super::cancel::{cancelled_reply, take_cancelled};
use super::document::{byte_offset_to_position, byte_span_to_range, position_to_offset};
use super::kinds::completion_kind_to_lsp;
#[cfg(not(target_arch = "wasm32"))]
use super::kinds::object_type_to_symbol_kind;
#[cfg(not(target_arch = "wasm32"))]
use super::{LspServer, parse};

#[cfg(not(target_arch = "wasm32"))]
impl LspServer {
    pub(super) fn handle_completion(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: CompletionParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let Some(text) = guard.docs.get(&params.text_document.uri).map(|d| &d.text) else {
            return vec![Message::response(id, serde_json::json!([]))];
        };
        let cursor = position_to_offset(text, params.position);
        // Pure-CPU: holding the lock here is fine (no `.await`).
        let items = self.services.intellisense.complete_enriched(
            text,
            cursor,
            &guard.completion_catalog,
            &guard.completion_procedures,
            &guard.completion_rules,
        );
        let lsp_items = completion_to_lsp(&items);
        vec![Message::response(
            id,
            serde_json::to_value(&lsp_items).unwrap(),
        )]
    }

    pub(super) fn handle_hover(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: HoverParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let Some(text) = guard.docs.get(&params.text_document.uri).map(|d| &d.text) else {
            return vec![Message::response(id, serde_json::Value::Null)];
        };
        let cursor = position_to_offset(text, params.position);
        // Hover: if the cursor is on a procedure name (schema.name or bare
        // name matching a procedure), show its parameter list.
        let hover = build_hover(text, cursor, &guard.completion_procedures);
        vec![Message::response(
            id,
            hover
                .map(|h| serde_json::to_value(&h).unwrap())
                .unwrap_or(serde_json::Value::Null),
        )]
    }

    /// `textDocument/definition`: jump to the `CREATE [OR ALTER]` statement
    /// declaring the identifier under the cursor. Document-scoped by design —
    /// it searches the open documents' text (same-document declarations
    /// first), never the connected database, so it answers instantly and
    /// works offline. Params share the hover shape (a position in a doc).
    pub(super) fn handle_definition(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: HoverParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let Some(text) = guard.docs.get(&params.text_document.uri).map(|d| &d.text) else {
            return vec![Message::response(id, serde_json::json!([]))];
        };
        let cursor = position_to_offset(text, params.position);
        let Some(target) = identifier_at(text, cursor) else {
            return vec![Message::response(id, serde_json::json!([]))];
        };

        // Same document first, then the other open docs in stable uri order.
        let mut uris: Vec<String> = guard.docs.keys().cloned().collect();
        uris.sort();
        uris.retain(|u| *u != params.text_document.uri);
        uris.insert(0, params.text_document.uri.clone());
        let mut locations = Vec::new();
        for uri in &uris {
            let Some(doc) = guard.docs.get(uri) else {
                continue;
            };
            for span in definition_spans(&doc.text) {
                if span.normalized == target.normalized {
                    locations.push(Location {
                        uri: uri.clone(),
                        range: byte_span_to_range(&doc.text, span.start, span.end),
                    });
                }
            }
        }
        vec![Message::response(
            id,
            serde_json::to_value(&locations).unwrap(),
        )]
    }

    /// `textDocument/references`: every occurrence of the identifier under
    /// the cursor across the open documents, spelled bare, bracketed, quoted,
    /// or backticked. `includeDeclaration: false` drops occurrences that are
    /// `CREATE` declaration names.
    pub(super) fn handle_references(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: ReferenceParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let Some(text) = guard.docs.get(&params.text_document.uri).map(|d| &d.text) else {
            return vec![Message::response(id, serde_json::json!([]))];
        };
        let cursor = position_to_offset(text, params.position);
        let Some(target) = identifier_at(text, cursor) else {
            return vec![Message::response(id, serde_json::json!([]))];
        };

        let mut uris: Vec<&String> = guard.docs.keys().collect();
        uris.sort();
        let mut locations = Vec::new();
        for uri in uris {
            let doc = &guard.docs[uri];
            let declarations: Vec<(usize, usize)> = definition_spans(&doc.text)
                .into_iter()
                .filter(|d| d.normalized == target.normalized)
                .map(|d| (d.start, d.end))
                .collect();
            for occurrence in scan_identifiers(&doc.text) {
                if occurrence.normalized != target.normalized {
                    continue;
                }
                if !params.context.include_declaration
                    && declarations.contains(&(occurrence.start, occurrence.end))
                {
                    continue;
                }
                locations.push(Location {
                    uri: uri.clone(),
                    range: byte_span_to_range(&doc.text, occurrence.start, occurrence.end),
                });
            }
        }
        vec![Message::response(
            id,
            serde_json::to_value(&locations).unwrap(),
        )]
    }

    pub(super) fn spawn_workspace_symbol(&mut self, msg: Message, id: Id) {
        let params: WorkspaceSymbolParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => {
                self.send(Message::error(id, err.code, err.message));
                return;
            }
        };
        let profile = { self.shared.lock().unwrap().profile.clone() };
        let Some(profile) = profile else {
            self.send(Message::response(id, serde_json::json!([])));
            return;
        };
        let services = self.services.clone();
        let shared = self.shared.clone();
        let out = self.out();
        let handle = self.rt.spawn(async move {
            // Same pre-work cancellation probe as diagnostics: don't run the
            // catalog search for a request the client already abandoned.
            if let Some(reply) = cancelled_reply(&shared, &id) {
                let _ = out.send(reply);
                return;
            }
            let resp = match services
                .catalog
                .search_schema(&profile, &params.query, Some(50), Vec::new())
                .await
            {
                Ok(hits) => {
                    let symbols: Vec<SymbolInformation> = hits
                        .iter()
                        .map(|hit| SymbolInformation {
                            name: format!(
                                "{}{}",
                                hit.schema
                                    .as_deref()
                                    .map(|s| format!("{s}."))
                                    .unwrap_or_default(),
                                hit.name
                            ),
                            kind: object_type_to_symbol_kind(&hit.object_type),
                            location: Location {
                                uri: hit.object_key.clone(),
                                range: Range::default(),
                            },
                        })
                        .collect();
                    Message::response(id, serde_json::to_value(&symbols).unwrap())
                }
                Err(e) => Message::error(id, error_code::INTERNAL_ERROR, e.to_string()),
            };
            let resp = match take_cancelled(&shared, resp.id.as_ref()) {
                true => {
                    let id = resp.id.clone().unwrap_or(Id::Int(0));
                    Message::error(
                        id,
                        error_code::REQUEST_CANCELLED,
                        "request cancelled".to_string(),
                    )
                }
                false => resp,
            };
            let _ = out.send(resp);
        });
        self.pending.push(handle);
    }

    pub(super) fn handle_semantic_tokens(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: TextDocumentIdentifier = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let Some(text) = guard.docs.get(&params.uri).map(|d| &d.text) else {
            return vec![Message::response(
                id,
                serde_json::to_value(&SemanticTokens { data: Vec::new() }).unwrap(),
            )];
        };
        let dialect = guard
            .profile
            .as_ref()
            .map(|p| p.database_type.family())
            .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer);
        let tokens = semantic_tokens_for(text, dialect);
        vec![Message::response(
            id,
            serde_json::to_value(&tokens).unwrap(),
        )]
    }

    pub(super) fn handle_document_symbol(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: DocumentSymbolParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let dialect = guard
            .profile
            .as_ref()
            .map(|p| p.database_type.family())
            .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer);
        let Some(text) = guard
            .docs
            .get(&params.text_document.uri)
            .map(|d| d.text.clone())
        else {
            return vec![Message::response(id, serde_json::json!([]))];
        };
        drop(guard);
        let symbols = document_symbols(&text, dialect);
        vec![Message::response(
            id,
            serde_json::to_value(&symbols).unwrap(),
        )]
    }

    pub(super) fn handle_formatting(&self, msg: Message, id: Id) -> Vec<Message> {
        let params: DocumentFormattingParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        let guard = self.shared.lock().unwrap();
        let dialect = guard.profile.as_ref().map(|p| p.database_type.family());
        let Some(text) = guard
            .docs
            .get(&params.text_document.uri)
            .map(|d| d.text.clone())
        else {
            return vec![Message::response(id, serde_json::json!([]))];
        };
        drop(guard);
        let edits = format_document(&text, dialect, &params.options);
        vec![Message::response(id, serde_json::to_value(&edits).unwrap())]
    }
}

/// The semantic-token type legend advertised in `initialize` and consumed by
/// [`tokenize_semantic`]. The order is the contract: token-type indices in the
/// encoded `data` array refer to this list.
pub fn semantic_token_legend() -> Vec<String> {
    vec![
        "keyword".to_string(),
        "function".to_string(),
        "type".to_string(),
        "parameter".to_string(),
        "string".to_string(),
        "variable".to_string(),
        "comment".to_string(),
        "number".to_string(),
    ]
}

/// Map a batch of downstack [`CompletionItem`]s to the LSP wire shape, the same
/// way `handle_completion` does.
pub fn completion_to_lsp(items: &[CompletionItem]) -> Vec<CompletionItemLsp> {
    items
        .iter()
        .map(|item| CompletionItemLsp {
            label: item.label.clone(),
            kind: completion_kind_to_lsp(item.kind) as i32,
            insert_text: item.insert_text.clone(),
        })
        .collect()
}

/// Build the `textDocument/hover` result for a cursor position, mirroring
/// `handle_hover`. Returns `None` when the cursor is not on a known procedure
/// name (the server replies `null` in that case).
pub fn build_hover(text: &str, cursor: usize, procedures: &[ProcedureInfo]) -> Option<Hover> {
    let prefix = sqlly_intel::intellisense::current_prefix(text, cursor);
    procedures
        .iter()
        .find(|proc| {
            proc.qualified_name().eq_ignore_ascii_case(prefix)
                || proc.name.eq_ignore_ascii_case(prefix)
        })
        .map(|proc| Hover {
            contents: MarkupContent {
                kind: "markdown".to_string(),
                value: format!(
                    "**{}**\n\n```sql\n{}\n```\n\nParameters:\n{}",
                    proc.qualified_name(),
                    proc.declaration_snippet(),
                    proc.parameter_list_text(),
                ),
            },
            range: None,
        })
}

/// One identifier occurrence in a SQL script: its byte span (including any
/// bracket/quote delimiters) and its normalized name — delimiters stripped,
/// doubled escapes collapsed, case-folded — so `Users`, `[Users]`, `"users"`,
/// and `` `USERS` `` all compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentOccurrence {
    pub start: usize,
    pub end: usize,
    pub normalized: String,
    /// The previous non-whitespace byte was `.` — this occurrence continues a
    /// dotted chain (`dbo.Users`).
    pub dot_prefixed: bool,
}

/// Scan `text` for identifier occurrences, skipping comments and string
/// literals. Purpose-built for definition/references: it does not classify
/// keywords (a keyword is just an identifier occurrence nobody asks about)
/// and it is dialect-permissive — brackets, double quotes, and backticks are
/// all identifier quoting, which can only over-match, never miss.
pub fn scan_identifiers(text: &str) -> Vec<IdentOccurrence> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut last_significant: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                last_significant = Some(b'/');
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if bytes.get(i) != Some(&b'\'') {
                            break;
                        }
                    }
                    i += 1;
                }
                last_significant = Some(b'\'');
            }
            b'[' | b'"' | b'`' => {
                let closer = if b == b'[' { b']' } else { b };
                let start = i;
                i += 1;
                let content_start = i;
                while i < bytes.len() {
                    if bytes[i] == closer {
                        i += 1;
                        if bytes.get(i) != Some(&closer) {
                            break;
                        }
                    }
                    i += 1;
                }
                let content_end = i.saturating_sub(1).max(content_start);
                let raw = &text[content_start..content_end.min(text.len())];
                let closer_str = (closer as char).to_string();
                let normalized = raw
                    .replace(&closer_str.repeat(2), &closer_str)
                    .to_lowercase();
                if !normalized.is_empty() {
                    out.push(IdentOccurrence {
                        start,
                        end: i,
                        normalized,
                        dot_prefixed: last_significant == Some(b'.'),
                    });
                }
                last_significant = Some(closer);
            }
            _ if b.is_ascii_alphabetic() || b == b'_' || b == b'#' || b == b'@' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || matches!(bytes[i], b'_' | b'$' | b'#' | b'@'))
                {
                    i += 1;
                }
                out.push(IdentOccurrence {
                    start,
                    end: i,
                    normalized: text[start..i].to_lowercase(),
                    dot_prefixed: last_significant == Some(b'.'),
                });
                last_significant = Some(b'a');
            }
            _ => {
                if !b.is_ascii_whitespace() {
                    last_significant = Some(b);
                }
                i += 1;
            }
        }
    }
    out
}

/// The identifier occurrence under (or immediately left of) `offset`.
pub fn identifier_at(text: &str, offset: usize) -> Option<IdentOccurrence> {
    scan_identifiers(text)
        .into_iter()
        .find(|ident| ident.start <= offset && offset <= ident.end)
}

/// Object kinds a `CREATE [OR ALTER] <kind>` statement can declare in a
/// script, plus the modifier words that may sit between `CREATE` and the kind.
const DEFINABLE_KINDS: &[&str] = &[
    "table",
    "view",
    "procedure",
    "proc",
    "function",
    "trigger",
    "index",
    "sequence",
    "type",
    "schema",
    "synonym",
    "database",
];
const CREATE_MODIFIERS: &[&str] = &[
    "unique",
    "clustered",
    "nonclustered",
    "materialized",
    "temporary",
    "temp",
    "global",
    "columnstore",
];

/// Byte spans of the names DECLARED in `text`: for every
/// `CREATE [OR ALTER] <kind> [db.][schema.]<name>`, the final name segment of
/// the dotted chain. `ALTER <kind>` also counts — it is where the current
/// definition lives when a script only alters.
pub fn definition_spans(text: &str) -> Vec<IdentOccurrence> {
    let idents = scan_identifiers(text);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < idents.len() {
        let verb = idents[i].normalized.as_str();
        if verb != "create" && verb != "alter" {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if verb == "create"
            && idents.get(j).is_some_and(|x| x.normalized == "or")
            && idents.get(j + 1).is_some_and(|x| x.normalized == "alter")
        {
            j += 2;
        }
        while idents
            .get(j)
            .is_some_and(|x| CREATE_MODIFIERS.contains(&x.normalized.as_str()))
        {
            j += 1;
        }
        let Some(kind) = idents.get(j) else {
            break;
        };
        if !DEFINABLE_KINDS.contains(&kind.normalized.as_str()) {
            i += 1;
            continue;
        }
        // The declared name is the last segment of the dotted chain that
        // starts right after the kind.
        if let Some(first) = idents.get(j + 1) {
            if first.dot_prefixed {
                // `CREATE TABLE .foo` — malformed; skip.
                i = j + 1;
                continue;
            }
            let mut last = j + 1;
            while idents.get(last + 1).is_some_and(|x| x.dot_prefixed) {
                last += 1;
            }
            out.push(idents[last].clone());
            i = last + 1;
        } else {
            break;
        }
    }
    out
}

/// Encode the semantic tokens for `text` against the server's legend, the same
/// way `handle_semantic_tokens` does.
pub fn semantic_tokens(text: &str) -> SemanticTokens {
    semantic_tokens_for(text, sqlly_lang::sqlmodel::Dialect::SqlServer)
}

/// [`semantic_tokens`] in an explicit dialect (quote forms and keyword tables
/// differ per engine).
pub fn semantic_tokens_for(text: &str, dialect: sqlly_lang::sqlmodel::Dialect) -> SemanticTokens {
    let legend = semantic_token_legend();
    let data = tokenize_semantic_for(text, &legend, dialect);
    SemanticTokens { data }
}

/// A best-effort semantic-token encoder. Emits the LSP relative-position
/// `deltaLine, deltaStart, length, tokenType, tokenModifiers` tuples for SQL
/// keywords.
pub fn tokenize_semantic(text: &str, legend: &[String]) -> Vec<u32> {
    tokenize_semantic_for(text, legend, sqlly_lang::sqlmodel::Dialect::SqlServer)
}

/// Parser-backed semantic classification: the document is lexed once with the
/// real SQL lexer, so keywords inside strings or comments are never
/// highlighted, and strings, comments, numbers, parameters, and function
/// names classify as themselves. Positions are UTF-16; multi-line tokens
/// (block comments, multi-line strings) are split into one segment per line
/// as the LSP encoding requires.
pub fn tokenize_semantic_for(
    text: &str,
    legend: &[String],
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> Vec<u32> {
    use sqlly_lang::sqllex::{StrictLexer, TokenKind};
    let type_index =
        |name: &str| -> Option<u32> { legend.iter().position(|t| t == name).map(|i| i as u32) };
    let keyword_type = type_index("keyword");
    let function_type = type_index("function");
    let parameter_type = type_index("parameter");
    let string_type = type_index("string");
    let comment_type = type_index("comment");
    let number_type = type_index("number");

    let tokens = StrictLexer::tokenize(text, dialect);
    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let classified = match token.kind {
            TokenKind::Keyword => keyword_type,
            TokenKind::StringLit(_) => string_type,
            TokenKind::Comment(_) => comment_type,
            TokenKind::Number(_) => number_type,
            TokenKind::ParamMarker(_) => parameter_type,
            TokenKind::Ident(_) => {
                if tokens
                    .get(i + 1)
                    .map(|next| next.kind == TokenKind::LParen)
                    .unwrap_or(false)
                {
                    function_type
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(token_type) = classified
            && token.span.end > token.span.start
        {
            spans.push((token.span.start, token.span.end, token_type));
        }
    }

    // Line index for byte → (line, UTF-16 column) conversion.
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        )
        .collect();
    let line_of = |byte: usize| match line_starts.binary_search(&byte) {
        Ok(line) => line,
        Err(line) => line - 1,
    };
    let utf16_col =
        |line: usize, byte: usize| text[line_starts[line]..byte].encode_utf16().count() as u32;

    let mut data = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for (start, end, token_type) in spans {
        let mut segment_start = start;
        while segment_start < end {
            let line = line_of(segment_start);
            // The byte where this line's content ends (the newline, or EOF).
            let mut line_end = line_starts
                .get(line + 1)
                .map(|next| next - 1)
                .unwrap_or(text.len());
            if line_end > segment_start
                && text.as_bytes().get(line_end.wrapping_sub(1)) == Some(&b'\r')
            {
                line_end -= 1;
            }
            let segment_end = end.min(line_end.max(segment_start));
            if segment_end > segment_start {
                let start_col = utf16_col(line, segment_start);
                let length = text[segment_start..segment_end].encode_utf16().count() as u32;
                let line_no = line as u32;
                let delta_line = line_no - prev_line;
                let delta_start = if delta_line == 0 {
                    start_col - prev_start
                } else {
                    start_col
                };
                data.extend_from_slice(&[delta_line, delta_start, length, token_type, 0]);
                prev_line = line_no;
                prev_start = start_col;
            }
            // Jump past this line (its newline included) toward the token end.
            let next_line_start = line_starts.get(line + 1).copied().unwrap_or(text.len());
            if next_line_start <= segment_start {
                break;
            }
            segment_start = next_line_start.min(end.max(next_line_start));
            if segment_end == end {
                break;
            }
        }
    }
    data
}

/// One flat document symbol per statement: DDL statements name their object,
/// DML statements name their verb plus first table. Spans are the parser's
/// statement spans, converted to UTF-16 positions.
pub fn document_symbols(text: &str, dialect: sqlly_lang::sqlmodel::Dialect) -> Vec<DocumentSymbol> {
    use sqlly_lang::sqlparse::StatementNode;
    // LSP SymbolKind constants.
    const KIND_METHOD: i32 = 6;
    const KIND_INTERFACE: i32 = 11;
    const KIND_FUNCTION: i32 = 12;
    const KIND_STRUCT: i32 = 23;

    let tree = sqlly_lang::sqlparse::parse(text, dialect);
    let mut symbols = Vec::new();
    for statement in &tree.statements {
        let (name, kind, span) = match statement {
            StatementNode::CreateTable(node) => (
                format!("CREATE TABLE {}", node.reference.display()),
                KIND_STRUCT,
                node.span,
            ),
            StatementNode::CreateView(node) => (
                format!("CREATE VIEW {}", node.reference.display()),
                KIND_INTERFACE,
                node.span,
            ),
            StatementNode::CreateRoutine(node) => (
                format!("CREATE {}", node.reference.display()),
                KIND_FUNCTION,
                node.span,
            ),
            StatementNode::Select(node) => {
                let target = node
                    .tables
                    .first()
                    .map(|t| format!(" {}", t.reference.display()))
                    .unwrap_or_default();
                (format!("SELECT{target}"), KIND_METHOD, node.span)
            }
            StatementNode::Insert(node) => (
                format!("INSERT {}", node.target.reference.display()),
                KIND_METHOD,
                node.span,
            ),
            StatementNode::Update(node) => (
                format!("UPDATE {}", node.target.reference.display()),
                KIND_METHOD,
                node.span,
            ),
            StatementNode::Delete(node) => {
                let target = node
                    .tables
                    .first()
                    .map(|t| format!(" {}", t.reference.display()))
                    .unwrap_or_default();
                (format!("DELETE{target}"), KIND_METHOD, node.span)
            }
            StatementNode::Exec(node) => (
                format!("EXEC {}", node.target.display()),
                KIND_METHOD,
                node.span,
            ),
            StatementNode::Opaque { span, head } => (head.to_uppercase(), KIND_METHOD, *span),
        };
        let range = Range {
            start: byte_offset_to_position(text, span.start),
            end: byte_offset_to_position(text, span.end),
        };
        symbols.push(DocumentSymbol {
            name,
            kind,
            range,
            selection_range: range,
        });
    }
    symbols
}

/// Format the whole document with the shared SQL formatter (defaults +
/// AutoDetect, overridden by the connection's dialect when one is set), as a
/// single full-document `TextEdit`. Unchanged output yields no edits. The
/// formatter is infallible by contract (re-lex safety net), so this never
/// mangles SQL.
pub fn format_document(
    text: &str,
    dialect: Option<sqlly_lang::sqlmodel::Dialect>,
    options: &FormattingOptions,
) -> Vec<TextEdit> {
    use sqlly_lang::sqlformat::{EditorIndent, IndentStyle, SqlDialect, SqlFormattingPreferences};
    let mut prefs = SqlFormattingPreferences::default();
    if let Some(dialect) = dialect {
        prefs.dialect = match dialect {
            sqlly_lang::sqlmodel::Dialect::Postgres => SqlDialect::Postgresql,
            sqlly_lang::sqlmodel::Dialect::MySql => SqlDialect::Mysql,
            sqlly_lang::sqlmodel::Dialect::Sqlite => SqlDialect::Sqlite,
            _ => SqlDialect::AutoDetect,
        };
    }
    let editor_indent = options.tab_size.map(|tab_size| EditorIndent {
        style: if options.insert_spaces.unwrap_or(true) {
            IndentStyle::Spaces
        } else {
            IndentStyle::Tabs
        },
        width: tab_size.clamp(1, 16) as i32,
    });
    let formatted = sqlly_lang::sqlformat::format_sql(text, &prefs, editor_indent);
    if formatted == text {
        return Vec::new();
    }
    vec![TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: byte_offset_to_position(text, text.len()),
        },
        new_text: formatted,
    }]
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn semantic_tokens_are_lexer_backed() {
        let legend = semantic_token_legend();
        let comment_type = legend.iter().position(|t| t == "comment").unwrap() as u32;
        let keyword_type = legend.iter().position(|t| t == "keyword").unwrap() as u32;
        let string_type = legend.iter().position(|t| t == "string").unwrap() as u32;

        // `select` inside the string must NOT classify as a keyword; the
        // string and comment classify as themselves.
        let data = tokenize_semantic_for(
            "SELECT 'select' -- select later\n",
            &legend,
            sqlly_lang::sqlmodel::Dialect::SqlServer,
        );
        let entries: Vec<[u32; 5]> = data
            .chunks(5)
            .map(|c| [c[0], c[1], c[2], c[3], c[4]])
            .collect();
        let types: Vec<u32> = entries.iter().map(|e| e[3]).collect();
        assert!(types.contains(&keyword_type), "{entries:?}");
        assert!(types.contains(&string_type), "{entries:?}");
        assert!(types.contains(&comment_type), "{entries:?}");
        // Exactly one keyword: the SELECT. The quoted one stayed a string.
        assert_eq!(
            types.iter().filter(|t| **t == keyword_type).count(),
            1,
            "{entries:?}"
        );
    }

    #[test]
    fn multi_line_comments_split_per_line() {
        let legend = semantic_token_legend();
        let comment_type = legend.iter().position(|t| t == "comment").unwrap() as u32;
        let data = tokenize_semantic_for(
            "/* one\ntwo */ SELECT 1",
            &legend,
            sqlly_lang::sqlmodel::Dialect::SqlServer,
        );
        let comment_segments = data.chunks(5).filter(|c| c[3] == comment_type).count();
        assert_eq!(
            comment_segments, 2,
            "block comment must emit one segment per line: {data:?}"
        );
    }

    #[test]
    fn document_symbols_name_statements() {
        let symbols = document_symbols(
            "SELECT 1 FROM dbo.users; CREATE TABLE dbo.t (id INT); UPDATE dbo.users SET a = 1 WHERE id = 2",
            sqlly_lang::sqlmodel::Dialect::SqlServer,
        );
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["SELECT dbo.users", "CREATE TABLE dbo.t", "UPDATE dbo.users"],
            "{symbols:?}"
        );
    }

    #[test]
    fn formatting_returns_full_document_edit_or_nothing() {
        let edits = format_document(
            "select 1 from dbo.users where id=2",
            None,
            &FormattingOptions::default(),
        );
        assert_eq!(edits.len(), 1, "{edits:?}");
        let formatted = &edits[0].new_text;
        assert_ne!(formatted, "select 1 from dbo.users where id=2");
        // Formatting the formatter's own output converges to no edits.
        let again = format_document(formatted, None, &FormattingOptions::default());
        assert!(again.is_empty(), "{again:?}");
    }

    /// Semantic-token columns are UTF-16 code units (the LSP default), not
    /// UTF-8 bytes: a 2-byte `ż` and a 4-byte `𝄞` before a keyword must shift
    /// its column by 1 and 2 respectively. Runs of whitespace and adjacent
    /// punctuation must not drift positions either.
    #[test]
    fn semantic_token_columns_are_utf16() {
        let legend = vec!["keyword".to_string()];
        // `𝄞` = 2 UTF-16 units; two spaces after it; FROM glued to `(`.
        let data = tokenize_semantic("SELECT 𝄞  FROM(x)", &legend);
        // Two keywords: SELECT at col 0 len 6, FROM at UTF-16 col 11
        // (SELECT=0..6, space=6, 𝄞=7..9, spaces=9..11) → delta_start 11.
        assert_eq!(data, vec![0, 0, 6, 0, 0, 0, 11, 4, 0, 0]);
    }
}
