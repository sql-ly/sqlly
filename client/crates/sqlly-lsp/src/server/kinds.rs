//! Mappings from downstack enums onto LSP wire kinds.

use sqlly_intel::intellisense::CompletionKind;

use crate::types::CompletionItemKind;

/// Map a downstack `CompletionKind` to an LSP `CompletionItemKind` integer.
pub fn completion_kind_to_lsp(kind: CompletionKind) -> CompletionItemKind {
    use CompletionKind::*;
    match kind {
        Keyword => CompletionItemKind::Keyword,
        Database | Schema => CompletionItemKind::Module,
        Table => CompletionItemKind::Class,
        Column => CompletionItemKind::Field,
        Function => CompletionItemKind::Function,
        Procedure => CompletionItemKind::Function,
        Type => CompletionItemKind::TypeParameter,
        Parameter | ResultSetColumn => CompletionItemKind::Variable,
        Snippet => CompletionItemKind::Snippet,
        Value => CompletionItemKind::Variable,
        Join => CompletionItemKind::Keyword,
    }
}

/// Map a catalog object_type string to an LSP SymbolKind integer.
pub(super) fn object_type_to_symbol_kind(object_type: &str) -> i32 {
    match object_type.to_ascii_lowercase().as_str() {
        "table" | "view" => 5, // Class
        "procedure" => 12,     // Function
        "function" => 12,      // Function
        "schema" => 3,         // Namespace
        "type" => 26,          // TypeParameter
        _ => 13,               // Variable
    }
}
