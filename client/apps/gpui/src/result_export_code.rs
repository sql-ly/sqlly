//! Result-set export to programming-language code snippets.
//!
//! Each supported language emits a record/type definition for one result row
//! plus a collection literal containing every row's data. All functions are
//! pure and take `&[ResultColumn]` + `&[Vec<CellValue>]` so they can be
//! unit-tested without any GPUI types, mirroring `result_export`.
//!
//! Type mapping: SQL types are bucketed into integer, float, boolean, and
//! text categories. Binary and date/time columns map to the language's string
//! type (binary as `0x...` hex, date/time as its display text) to keep the
//! emitted snippets self-contained and dependency-free.

use sqlly_core::{CellValue, ResultColumn};

use crate::codegen_types::{self, CoarseCategory, IdentCase, sanitize_ident, unique_idents};

/// Programming languages available for code-snippet export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeLanguage {
    CSharp,
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Kotlin,
    Swift,
    Java,
    Go,
    Php,
    Cpp,
}

impl CodeLanguage {
    /// All languages, in menu display order.
    pub fn all() -> &'static [CodeLanguage] {
        &[
            CodeLanguage::CSharp,
            CodeLanguage::Python,
            CodeLanguage::Rust,
            CodeLanguage::JavaScript,
            CodeLanguage::TypeScript,
            CodeLanguage::Kotlin,
            CodeLanguage::Swift,
            CodeLanguage::Java,
            CodeLanguage::Go,
            CodeLanguage::Php,
            CodeLanguage::Cpp,
        ]
    }

    /// Human-readable label for menus (e.g. "C#", "C++").
    pub fn label(self) -> &'static str {
        match self {
            CodeLanguage::CSharp => "C#",
            CodeLanguage::Python => "Python",
            CodeLanguage::Rust => "Rust",
            CodeLanguage::JavaScript => "JavaScript",
            CodeLanguage::TypeScript => "TypeScript",
            CodeLanguage::Kotlin => "Kotlin",
            CodeLanguage::Swift => "Swift",
            CodeLanguage::Java => "Java",
            CodeLanguage::Go => "Go",
            CodeLanguage::Php => "PHP",
            CodeLanguage::Cpp => "C++",
        }
    }

    /// File extension for save-as dialogs.
    pub fn extension(self) -> &'static str {
        match self {
            CodeLanguage::CSharp => "cs",
            CodeLanguage::Python => "py",
            CodeLanguage::Rust => "rs",
            CodeLanguage::JavaScript => "js",
            CodeLanguage::TypeScript => "ts",
            CodeLanguage::Kotlin => "kt",
            CodeLanguage::Swift => "swift",
            CodeLanguage::Java => "java",
            CodeLanguage::Go => "go",
            CodeLanguage::Php => "php",
            CodeLanguage::Cpp => "cpp",
        }
    }
}

/// The row exporter only distinguishes numbers, booleans, and text; the
/// finer categories the shared module knows (dates, decimals, binary, JSON)
/// fold into these via [`crate::codegen_types::SqlTypeCategory::coarse`].
type TypeCategory = CoarseCategory;

/// Infers the type bucket from a SQL type name (`"nvarchar(50)"`, `"int"`,
/// `"decimal(10,2)"`, ...). Binary and date/time types intentionally fall
/// into `Text` (see module docs).
fn categorize(data_type: &str) -> TypeCategory {
    codegen_types::categorize(data_type).coarse()
}

/// Rows with the text cells of numeric columns converted to typed numbers.
/// Decimal and money arrive as text from the TDS driver, and the PostgreSQL
/// and MySQL text protocols deliver every value as text; the generated code
/// declares those fields as numbers, so their literals must be numbers too,
/// not quoted strings. Text that is not a plain numeric literal is left
/// alone. Exact decimals go through `f64` here because the generated field
/// type is a double anyway.
fn typed_numeric_rows(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Vec<Vec<CellValue>> {
    let numeric: Vec<bool> = columns
        .iter()
        .map(|col| {
            matches!(
                categorize(&col.data_type),
                TypeCategory::Int | TypeCategory::Float
            )
        })
        .collect();
    if !numeric.iter().any(|flag| *flag) {
        return rows.to_vec();
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, value)| match value {
                    CellValue::String(text) if numeric.get(i).copied().unwrap_or(false) => {
                        typed_numeric_cell(text).unwrap_or_else(|| value.clone())
                    }
                    other => other.clone(),
                })
                .collect()
        })
        .collect()
}

/// `text` as an `I64` when it is an integer literal that fits, as a finite
/// `F64` when it is any other plain numeric literal, `None` otherwise.
fn typed_numeric_cell(text: &str) -> Option<CellValue> {
    let literal = crate::result_cell_format::numeric_literal(text)?;
    if let Ok(n) = literal.parse::<i64>() {
        return Some(CellValue::I64(n));
    }
    literal
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(CellValue::F64)
}

/// A single record field: sanitized identifier, type bucket, nullability.
struct Field {
    ident: String,
    cat: TypeCategory,
    nullable: bool,
}

/// Builds the field list for a result set: unique sanitized identifiers, type
/// buckets, and nullability. A column counts as nullable when declared
/// nullable, when any row actually contains NULL in it, or when any row holds
/// a non-finite float — the literal emitters render NaN/±infinity as the
/// language's null, so the field type has to admit one.
fn fields_for(columns: &[ResultColumn], rows: &[Vec<CellValue>], case: IdentCase) -> Vec<Field> {
    let idents = unique_idents(
        columns
            .iter()
            .map(|col| sanitize_ident(&col.name, case))
            .collect(),
    );
    columns
        .iter()
        .zip(idents)
        .enumerate()
        .map(|(i, (col, ident))| {
            let needs_null = rows.iter().any(|r| match r.get(i) {
                Some(CellValue::Null) | None => true,
                Some(CellValue::F64(v)) => !v.is_finite(),
                _ => false,
            });
            Field {
                ident,
                cat: categorize(&col.data_type),
                nullable: col.nullable || needs_null,
            }
        })
        .collect()
}

/// C-style string escaping, valid in every supported language.
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", escape_string(s))
}

/// Renders a float with a guaranteed decimal point so it stays a float
/// literal in languages with separate int/float syntax.
fn float_text(v: f64) -> String {
    let s = v.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{}.0", s)
    }
}

fn hex_string(bytes: &[u8]) -> String {
    format!(
        "0x{}",
        bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<String>()
    )
}

/// Literal shared by C#, JavaScript, TypeScript, Kotlin, Swift, Java, and
/// PHP: `null` for NULL, an optional suffix on integer literals (`"L"` for
/// Java/Kotlin longs), plain booleans, quoted strings, hex for bytes.
fn base_literal(value: &CellValue, int_suffix: &str) -> String {
    match value {
        CellValue::Null => "null".to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => format!("{}{}", n, int_suffix),
        CellValue::F64(v) => {
            if v.is_finite() {
                float_text(*v)
            } else {
                "null".to_string()
            }
        }
        CellValue::String(s) => quoted(s),
        CellValue::Bytes(b) => quoted(&hex_string(b)),
    }
}

/// [`base_literal`] for languages whose double-quoted strings interpolate
/// `$name` / `${expr}` / `{$expr}` (Kotlin, PHP): every `$` is escaped as
/// `\$`, so a database value stays text instead of becoming an expression
/// that runs with the generated code. Only string literals can contain `$`
/// (numbers, booleans, and hex bytes never do).
fn interpolation_safe_literal(value: &CellValue, int_suffix: &str) -> String {
    base_literal(value, int_suffix).replace('$', "\\$")
}

/// Generates the full code snippet for `language` from a result set.
/// Returns an empty string when there are no columns.
pub fn generate(
    language: CodeLanguage,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let rows = &typed_numeric_rows(columns, rows);
    match language {
        CodeLanguage::CSharp => csharp(columns, rows),
        CodeLanguage::Python => python(columns, rows),
        CodeLanguage::Rust => rust(columns, rows),
        CodeLanguage::JavaScript => javascript(columns, rows),
        CodeLanguage::TypeScript => typescript(columns, rows),
        CodeLanguage::Kotlin => kotlin(columns, rows),
        CodeLanguage::Swift => swift(columns, rows),
        CodeLanguage::Java => java(columns, rows),
        CodeLanguage::Go => go(columns, rows),
        CodeLanguage::Php => php(columns, rows),
        CodeLanguage::Cpp => cpp(columns, rows),
    }
}

fn csharp_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "long",
        TypeCategory::Float => "double",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "string",
    };
    if f.nullable {
        format!("{}?", base)
    } else {
        base.to_string()
    }
}

fn csharp(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Pascal);
    let mut out = String::from("// C# export — record type + data array.\npublic record Row(\n");
    let params: Vec<String> = fields
        .iter()
        .map(|f| format!("    {} {}", csharp_type(f), f.ident))
        .collect();
    out.push_str(&params.join(",\n"));
    out.push_str("\n);\n\nvar rows = new Row[]\n{");
    for row in rows {
        let args: Vec<String> = (0..fields.len())
            .map(|i| base_literal(row.get(i).unwrap_or(&CellValue::Null), ""))
            .collect();
        out.push_str(&format!("\n    new Row({}),", args.join(", ")));
    }
    out.push_str("\n};\n");
    out
}

fn python_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "int",
        TypeCategory::Float => "float",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "str",
    };
    if f.nullable {
        format!("Optional[{}]", base)
    } else {
        base.to_string()
    }
}

fn python_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "None".to_string(),
        CellValue::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        other => base_literal(other, ""),
    }
}

fn python(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Snake);
    let mut out = String::from(
        "# Python export — dataclass + list of rows.\nfrom dataclasses import dataclass\nfrom typing import Optional\n\n@dataclass\nclass Row:\n",
    );
    for f in &fields {
        out.push_str(&format!("    {}: {}\n", f.ident, python_type(f)));
    }
    out.push_str("\nrows = [");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}={}",
                    f.ident,
                    python_literal(row.get(i).unwrap_or(&CellValue::Null))
                )
            })
            .collect();
        out.push_str(&format!("\n    Row({}),", args.join(", ")));
    }
    out.push_str("\n]\n");
    out
}

fn rust_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "i64",
        TypeCategory::Float => "f64",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "String",
    };
    if f.nullable {
        format!("Option<{}>", base)
    } else {
        base.to_string()
    }
}

fn rust_literal(value: &CellValue, f: &Field) -> String {
    if *value == CellValue::Null {
        return "None".to_string();
    }
    let inner = match value {
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                float_text(*v)
            } else {
                return "None".to_string();
            }
        }
        CellValue::String(s) => format!("{}.to_string()", quoted(s)),
        CellValue::Bytes(b) => format!("{}.to_string()", quoted(&hex_string(b))),
        CellValue::Null => unreachable!(),
    };
    if f.nullable {
        format!("Some({})", inner)
    } else {
        inner
    }
}

fn rust(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Snake);
    let mut out = String::from(
        "// Rust export — struct + Vec of rows.\n#[derive(Debug, Clone)]\n#[allow(dead_code)]\nstruct Row {\n",
    );
    for f in &fields {
        out.push_str(&format!("    {}: {},\n", f.ident, rust_type(f)));
    }
    out.push_str("}\n\nlet rows = vec![");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}: {}",
                    f.ident,
                    rust_literal(row.get(i).unwrap_or(&CellValue::Null), f)
                )
            })
            .collect();
        out.push_str(&format!("\n    Row {{ {} }},", args.join(", ")));
    }
    out.push_str("\n];\n");
    out
}

fn javascript(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out = String::from(
        "// JavaScript export — JSDoc-typed record + array.\n/**\n * @typedef {Object} Row\n",
    );
    for f in &fields {
        let ty = match f.cat {
            TypeCategory::Int | TypeCategory::Float => "number",
            TypeCategory::Bool => "boolean",
            TypeCategory::Text => "string",
        };
        let ty = if f.nullable {
            format!("{}|null", ty)
        } else {
            ty.to_string()
        };
        out.push_str(&format!(" * @property {{{}}} {}\n", ty, f.ident));
    }
    out.push_str(" */\n\n/** @type {Row[]} */\nconst rows = [");
    out.push_str(&js_object_rows(&fields, rows));
    out
}

fn typescript(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out =
        String::from("// TypeScript export — interface + typed array.\ninterface Row {\n");
    for f in &fields {
        let ty = match f.cat {
            TypeCategory::Int | TypeCategory::Float => "number",
            TypeCategory::Bool => "boolean",
            TypeCategory::Text => "string",
        };
        let ty = if f.nullable {
            format!("{} | null", ty)
        } else {
            ty.to_string()
        };
        out.push_str(&format!("  {}: {};\n", f.ident, ty));
    }
    out.push_str("}\n\nconst rows: Row[] = [");
    out.push_str(&js_object_rows(&fields, rows));
    out
}

/// Shared object-literal rows for JavaScript and TypeScript, including the
/// closing `];`.
fn js_object_rows(fields: &[Field], rows: &[Vec<CellValue>]) -> String {
    let mut out = String::new();
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}: {}",
                    f.ident,
                    base_literal(row.get(i).unwrap_or(&CellValue::Null), "")
                )
            })
            .collect();
        out.push_str(&format!("\n  {{ {} }},", args.join(", ")));
    }
    out.push_str("\n];\n");
    out
}

fn kotlin_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "Long",
        TypeCategory::Float => "Double",
        TypeCategory::Bool => "Boolean",
        TypeCategory::Text => "String",
    };
    if f.nullable {
        format!("{}?", base)
    } else {
        base.to_string()
    }
}

fn kotlin(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out = String::from("// Kotlin export — data class + list.\ndata class Row(\n");
    let params: Vec<String> = fields
        .iter()
        .map(|f| format!("    val {}: {}", f.ident, kotlin_type(f)))
        .collect();
    out.push_str(&params.join(",\n"));
    out.push_str("\n)\n\nval rows = listOf(");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{} = {}",
                    f.ident,
                    interpolation_safe_literal(row.get(i).unwrap_or(&CellValue::Null), "L")
                )
            })
            .collect();
        out.push_str(&format!("\n    Row({}),", args.join(", ")));
    }
    out.push_str("\n)\n");
    out
}

fn swift_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "Int",
        TypeCategory::Float => "Double",
        TypeCategory::Bool => "Bool",
        TypeCategory::Text => "String",
    };
    if f.nullable {
        format!("{}?", base)
    } else {
        base.to_string()
    }
}

fn swift_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "nil".to_string(),
        other => base_literal(other, ""),
    }
}

fn swift(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out = String::from("// Swift export — struct + array.\nstruct Row {\n");
    for f in &fields {
        out.push_str(&format!("    let {}: {}\n", f.ident, swift_type(f)));
    }
    out.push_str("}\n\nlet rows: [Row] = [");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}: {}",
                    f.ident,
                    swift_literal(row.get(i).unwrap_or(&CellValue::Null))
                )
            })
            .collect();
        out.push_str(&format!("\n    Row({}),", args.join(", ")));
    }
    out.push_str("\n]\n");
    out
}

fn java_type(f: &Field) -> &'static str {
    match f.cat {
        TypeCategory::Int => "Long",
        TypeCategory::Float => "Double",
        TypeCategory::Bool => "Boolean",
        TypeCategory::Text => "String",
    }
}

fn java(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out = String::from(
        "// Java export — record + List of rows.\nimport java.util.List;\n\npublic class ResultRows {\n    public record Row(",
    );
    let params: Vec<String> = fields
        .iter()
        .map(|f| format!("{} {}", java_type(f), f.ident))
        .collect();
    out.push_str(&params.join(", "));
    out.push_str(") {}\n\n    public static final List<Row> ROWS = List.of(");
    // Unlike the other languages' collection literals, Java's method-call
    // argument list rejects a trailing comma — join instead of appending.
    let entries: Vec<String> = rows
        .iter()
        .map(|row| {
            let args: Vec<String> = (0..fields.len())
                .map(|i| base_literal(row.get(i).unwrap_or(&CellValue::Null), "L"))
                .collect();
            format!("\n        new Row({})", args.join(", "))
        })
        .collect();
    out.push_str(&entries.join(","));
    out.push_str("\n    );\n}\n");
    out
}

fn go_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "int64",
        TypeCategory::Float => "float64",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "string",
    };
    if f.nullable {
        format!("*{}", base)
    } else {
        base.to_string()
    }
}

fn go_literal(value: &CellValue, f: &Field) -> String {
    if *value == CellValue::Null {
        return "nil".to_string();
    }
    let inner = match value {
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                float_text(*v)
            } else {
                return "nil".to_string();
            }
        }
        CellValue::String(s) => quoted(s),
        CellValue::Bytes(b) => quoted(&hex_string(b)),
        CellValue::Null => unreachable!(),
    };
    if f.nullable {
        format!("ptr({})", inner)
    } else {
        inner
    }
}

fn go(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Pascal);
    let needs_ptr = fields.iter().any(|f| f.nullable);
    let mut out = String::from(
        "// Go export — struct type + slice of rows.\npackage main\n\ntype Row struct {\n",
    );
    for f in &fields {
        out.push_str(&format!("\t{} {}\n", f.ident, go_type(f)));
    }
    out.push_str("}\n");
    if needs_ptr {
        out.push_str("\nfunc ptr[T any](v T) *T { return &v }\n");
    }
    out.push_str("\nvar rows = []Row{");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}: {}",
                    f.ident,
                    go_literal(row.get(i).unwrap_or(&CellValue::Null), f)
                )
            })
            .collect();
        out.push_str(&format!("\n\t{{{}}},", args.join(", ")));
    }
    out.push_str("\n}\n");
    out
}

fn php_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "int",
        TypeCategory::Float => "float",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "string",
    };
    if f.nullable {
        format!("?{}", base)
    } else {
        base.to_string()
    }
}

fn php(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Camel);
    let mut out = String::from(
        "<?php\n// PHP export — class + array of rows (named arguments, PHP 8+).\n\nclass Row {\n    public function __construct(\n",
    );
    for f in &fields {
        out.push_str(&format!("        public {} ${},\n", php_type(f), f.ident));
    }
    out.push_str("    ) {}\n}\n\n/** @var Row[] */\n$rows = [");
    for row in rows {
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}: {}",
                    f.ident,
                    interpolation_safe_literal(row.get(i).unwrap_or(&CellValue::Null), "")
                )
            })
            .collect();
        out.push_str(&format!("\n    new Row({}),", args.join(", ")));
    }
    out.push_str("\n];\n");
    out
}

fn cpp_type(f: &Field) -> String {
    let base = match f.cat {
        TypeCategory::Int => "int64_t",
        TypeCategory::Float => "double",
        TypeCategory::Bool => "bool",
        TypeCategory::Text => "std::string",
    };
    if f.nullable {
        format!("std::optional<{}>", base)
    } else {
        base.to_string()
    }
}

fn cpp_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "std::nullopt".to_string(),
        other => base_literal(other, ""),
    }
}

fn cpp(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let fields = fields_for(columns, rows, IdentCase::Snake);
    let mut out = String::from(
        "// C++ export — struct + vector of rows.\n#include <cstdint>\n#include <optional>\n#include <string>\n#include <vector>\n\nstruct Row {\n",
    );
    for f in &fields {
        out.push_str(&format!("    {} {};\n", cpp_type(f), f.ident));
    }
    out.push_str("};\n\nconst std::vector<Row> rows = {");
    for row in rows {
        let args: Vec<String> = (0..fields.len())
            .map(|i| cpp_literal(row.get(i).unwrap_or(&CellValue::Null)))
            .collect();
        out.push_str(&format!("\n    {{{}}},", args.join(", ")));
    }
    out.push_str("\n};\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kotlin and PHP double-quoted strings interpolate `$name` / `${expr}`
    /// / `{$expr}`: a database value must stay text in the generated code,
    /// never become an expression that runs when the snippet is compiled or
    /// executed.
    #[test]
    fn interpolating_languages_escape_dollar_signs() {
        let columns = cols(&[("note", "nvarchar(50)", false)]);
        let rows = vec![vec![CellValue::String(
            "${Runtime.getRuntime().exec(\"calc\")} {$x} $y".into(),
        )]];
        let kotlin = generate(CodeLanguage::Kotlin, &columns, &rows);
        assert!(
            kotlin.contains(r#""\${Runtime.getRuntime().exec(\"calc\")} {\$x} \$y""#),
            "{kotlin}"
        );
        let php = generate(CodeLanguage::Php, &columns, &rows);
        assert!(
            php.contains(r#""\${Runtime.getRuntime().exec(\"calc\")} {\$x} \$y""#),
            "{php}"
        );
        // Languages without interpolation keep `$` as-is.
        let java = generate(CodeLanguage::Java, &columns, &rows);
        assert!(java.contains(r#""${Runtime"#), "{java}");
    }

    fn cols(spec: &[(&str, &str, bool)]) -> Vec<ResultColumn> {
        spec.iter()
            .map(|(name, dt, nullable)| ResultColumn {
                name: name.to_string(),
                data_type: dt.to_string(),
                nullable: *nullable,
            })
            .collect()
    }

    fn sample_cols() -> Vec<ResultColumn> {
        cols(&[("id", "int", false), ("name", "varchar(50)", true)])
    }

    fn sample_rows() -> Vec<Vec<CellValue>> {
        vec![
            vec![CellValue::I64(1), CellValue::String("Alice".into())],
            vec![CellValue::I64(2), CellValue::Null],
        ]
    }

    #[test]
    fn code_language_all_has_eleven_entries() {
        assert_eq!(CodeLanguage::all().len(), 11);
        for lang in CodeLanguage::all() {
            assert!(!lang.label().is_empty());
            assert!(!lang.extension().is_empty());
        }
    }

    #[test]
    fn numeric_text_in_numeric_columns_becomes_number_literals() {
        let columns = vec![
            ResultColumn {
                name: "qty".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "price".into(),
                data_type: "decimal(10,2)".into(),
                nullable: false,
            },
            ResultColumn {
                name: "sku".into(),
                data_type: "varchar(20)".into(),
                nullable: false,
            },
        ];
        let rows = vec![vec![
            CellValue::String("42".into()),
            CellValue::String("12.50".into()),
            CellValue::String("00123".into()),
        ]];
        let typed = typed_numeric_rows(&columns, &rows);
        assert_eq!(typed[0][0], CellValue::I64(42));
        assert_eq!(typed[0][1], CellValue::F64(12.5));
        assert_eq!(typed[0][2], CellValue::String("00123".into()));
        let out = generate(CodeLanguage::CSharp, &columns, &rows);
        assert!(out.contains("new Row(42, 12.5, \"00123\")"), "{out}");
        let out = generate(CodeLanguage::Python, &columns, &rows);
        assert!(out.contains("qty=42, price=12.5, sku=\"00123\""), "{out}");
        // Text that is not a numeric literal stays a string even in a
        // numeric column.
        let rows = vec![vec![
            CellValue::String("N/A".into()),
            CellValue::String("$1.00".into()),
            CellValue::Null,
        ]];
        let typed = typed_numeric_rows(&columns, &rows);
        assert_eq!(typed[0][0], CellValue::String("N/A".into()));
        assert_eq!(typed[0][1], CellValue::String("$1.00".into()));
    }

    #[test]
    fn categorize_common_sql_types() {
        assert_eq!(categorize("int"), TypeCategory::Int);
        assert_eq!(categorize("bigint"), TypeCategory::Int);
        assert_eq!(categorize("nvarchar(50)"), TypeCategory::Text);
        assert_eq!(categorize("decimal(10,2)"), TypeCategory::Float);
        assert_eq!(categorize("float"), TypeCategory::Float);
        assert_eq!(categorize("bit"), TypeCategory::Bool);
        assert_eq!(categorize("datetime2"), TypeCategory::Text);
        assert_eq!(categorize("varbinary(max)"), TypeCategory::Text);
        assert_eq!(categorize("point"), TypeCategory::Text);
        assert_eq!(categorize(""), TypeCategory::Text);
        assert_eq!(categorize("(unknown)"), TypeCategory::Text);
    }

    #[test]
    fn csharp_emits_record_and_array() {
        let out = generate(CodeLanguage::CSharp, &sample_cols(), &sample_rows());
        assert!(out.contains("public record Row("), "{}", out);
        assert!(out.contains("long Id"), "{}", out);
        assert!(out.contains("string? Name"), "{}", out);
        assert!(out.contains("new Row(1, \"Alice\")"), "{}", out);
        assert!(out.contains("new Row(2, null)"), "{}", out);
    }

    #[test]
    fn python_emits_dataclass_and_list() {
        let out = generate(CodeLanguage::Python, &sample_cols(), &sample_rows());
        assert!(out.contains("@dataclass"), "{}", out);
        assert!(out.contains("class Row:"), "{}", out);
        assert!(out.contains("id: int"), "{}", out);
        assert!(out.contains("name: Optional[str]"), "{}", out);
        assert!(out.contains("Row(id=1, name=\"Alice\")"), "{}", out);
        assert!(out.contains("Row(id=2, name=None)"), "{}", out);
    }

    #[test]
    fn rust_emits_struct_and_vec() {
        let out = generate(CodeLanguage::Rust, &sample_cols(), &sample_rows());
        assert!(out.contains("struct Row {"), "{}", out);
        assert!(out.contains("id: i64,"), "{}", out);
        assert!(out.contains("name: Option<String>,"), "{}", out);
        assert!(
            out.contains("Row { id: 1, name: Some(\"Alice\".to_string()) }"),
            "{}",
            out
        );
        assert!(out.contains("Row { id: 2, name: None }"), "{}", out);
    }

    #[test]
    fn javascript_emits_typedef_and_array() {
        let out = generate(CodeLanguage::JavaScript, &sample_cols(), &sample_rows());
        assert!(out.contains("@typedef {Object} Row"), "{}", out);
        assert!(out.contains("@property {string|null} name"), "{}", out);
        assert!(out.contains("{ id: 1, name: \"Alice\" }"), "{}", out);
        assert!(out.contains("{ id: 2, name: null }"), "{}", out);
    }

    #[test]
    fn typescript_emits_interface_and_array() {
        let out = generate(CodeLanguage::TypeScript, &sample_cols(), &sample_rows());
        assert!(out.contains("interface Row {"), "{}", out);
        assert!(out.contains("id: number;"), "{}", out);
        assert!(out.contains("name: string | null;"), "{}", out);
        assert!(out.contains("const rows: Row[] = ["), "{}", out);
        assert!(out.contains("{ id: 2, name: null }"), "{}", out);
    }

    #[test]
    fn kotlin_emits_data_class() {
        let out = generate(CodeLanguage::Kotlin, &sample_cols(), &sample_rows());
        assert!(out.contains("data class Row("), "{}", out);
        assert!(out.contains("val id: Long"), "{}", out);
        assert!(out.contains("val name: String?"), "{}", out);
        assert!(out.contains("Row(id = 1L, name = \"Alice\")"), "{}", out);
        assert!(out.contains("Row(id = 2L, name = null)"), "{}", out);
    }

    #[test]
    fn swift_emits_struct_and_array() {
        let out = generate(CodeLanguage::Swift, &sample_cols(), &sample_rows());
        assert!(out.contains("struct Row {"), "{}", out);
        assert!(out.contains("let id: Int"), "{}", out);
        assert!(out.contains("let name: String?"), "{}", out);
        assert!(out.contains("Row(id: 1, name: \"Alice\")"), "{}", out);
        assert!(out.contains("Row(id: 2, name: nil)"), "{}", out);
    }

    #[test]
    fn java_emits_record_with_long_literals() {
        let out = generate(CodeLanguage::Java, &sample_cols(), &sample_rows());
        assert!(
            out.contains("public record Row(Long id, String name)"),
            "{}",
            out
        );
        assert!(out.contains("new Row(1L, \"Alice\")"), "{}", out);
        assert!(out.contains("new Row(2L, null)"), "{}", out);
    }

    #[test]
    fn go_emits_struct_slice_and_ptr_helper() {
        let out = generate(CodeLanguage::Go, &sample_cols(), &sample_rows());
        assert!(out.contains("package main"), "{}", out);
        assert!(out.contains("type Row struct {"), "{}", out);
        assert!(out.contains("\tId int64"), "{}", out);
        assert!(out.contains("\tName *string"), "{}", out);
        assert!(
            out.contains("func ptr[T any](v T) *T { return &v }"),
            "{}",
            out
        );
        assert!(out.contains("{Id: 1, Name: ptr(\"Alice\")}"), "{}", out);
        assert!(out.contains("{Id: 2, Name: nil}"), "{}", out);
    }

    #[test]
    fn go_omits_ptr_helper_when_no_nullable_fields() {
        let c = cols(&[("id", "int", false)]);
        let r = vec![vec![CellValue::I64(1)]];
        let out = generate(CodeLanguage::Go, &c, &r);
        assert!(!out.contains("func ptr"), "{}", out);
    }

    #[test]
    fn php_emits_promoted_constructor() {
        let out = generate(CodeLanguage::Php, &sample_cols(), &sample_rows());
        assert!(out.starts_with("<?php"), "{}", out);
        assert!(out.contains("public function __construct("), "{}", out);
        assert!(out.contains("public int $id,"), "{}", out);
        assert!(out.contains("public ?string $name,"), "{}", out);
        assert!(out.contains("new Row(id: 1, name: \"Alice\")"), "{}", out);
        assert!(out.contains("new Row(id: 2, name: null)"), "{}", out);
    }

    #[test]
    fn cpp_emits_optional_struct_and_vector() {
        let out = generate(CodeLanguage::Cpp, &sample_cols(), &sample_rows());
        assert!(out.contains("#include <vector>"), "{}", out);
        assert!(out.contains("int64_t id;"), "{}", out);
        assert!(out.contains("std::optional<std::string> name;"), "{}", out);
        assert!(out.contains("{1, \"Alice\"}"), "{}", out);
        assert!(out.contains("{2, std::nullopt}"), "{}", out);
    }

    #[test]
    fn null_in_non_nullable_column_makes_field_nullable() {
        let c = cols(&[("id", "int", false)]);
        let r = vec![vec![CellValue::Null]];
        let out = generate(CodeLanguage::TypeScript, &c, &r);
        assert!(out.contains("id: number | null;"), "{}", out);
    }

    #[test]
    fn java_rows_have_no_trailing_comma() {
        // Java's List.of(...) is a method call; a trailing comma after the
        // last argument is a syntax error (unlike the other languages'
        // collection literals).
        let out = generate(CodeLanguage::Java, &sample_cols(), &sample_rows());
        assert!(!out.contains("),\n    );"), "{}", out);
        assert!(out.contains(")\n    );"), "{}", out);
    }

    #[test]
    fn non_finite_float_makes_field_nullable() {
        // NaN/infinity render as the language's null, so the field type must
        // admit one even when the column has no NULLs.
        let c = cols(&[("ratio", "float", false)]);
        let r = vec![vec![CellValue::F64(f64::NAN)]];
        let out = generate(CodeLanguage::Rust, &c, &r);
        assert!(out.contains("ratio: Option<f64>,"), "{}", out);
        assert!(out.contains("ratio: None"), "{}", out);
        let out = generate(CodeLanguage::CSharp, &c, &r);
        assert!(out.contains("double? Ratio"), "{}", out);
    }

    #[test]
    fn duplicate_column_names_get_distinct_idents() {
        let c = cols(&[
            ("a b", "int", false),
            ("a_b", "int", false),
            ("a b", "int", false),
        ]);
        let out = generate(CodeLanguage::Python, &c, &[]);
        assert!(out.contains("a_b: int"), "{}", out);
        assert!(out.contains("a_b_2: int"), "{}", out);
        assert!(out.contains("a_b_3: int"), "{}", out);
    }

    #[test]
    fn strings_are_escaped() {
        let c = cols(&[("s", "text", false)]);
        let r = vec![vec![CellValue::String("say \"hi\"\nbye\\".into())]];
        for lang in CodeLanguage::all() {
            let out = generate(*lang, &c, &r);
            assert!(
                out.contains("\\\"hi\\\"\\nbye\\\\"),
                "{} output missing escapes: {}",
                lang.label(),
                out
            );
        }
    }

    #[test]
    fn bool_literals_match_language() {
        let c = cols(&[("b", "bit", false)]);
        let r = vec![vec![CellValue::Bool(true)]];
        assert!(generate(CodeLanguage::Python, &c, &r).contains("b=True"));
        assert!(generate(CodeLanguage::TypeScript, &c, &r).contains("b: true"));
        assert!(generate(CodeLanguage::Swift, &c, &r).contains("b: true"));
    }

    #[test]
    fn empty_columns_yield_empty_output() {
        for lang in CodeLanguage::all() {
            assert_eq!(generate(*lang, &[], &sample_rows()), "");
        }
    }

    #[test]
    fn empty_rows_still_emit_type_and_empty_collection() {
        for lang in CodeLanguage::all() {
            let out = generate(*lang, &sample_cols(), &[]);
            assert!(!out.is_empty(), "{} produced nothing", lang.label());
        }
    }

    #[test]
    fn every_language_produces_nonempty_output() {
        for lang in CodeLanguage::all() {
            let out = generate(*lang, &sample_cols(), &sample_rows());
            assert!(
                out.len() > 40,
                "{} output too short: {:?}",
                lang.label(),
                out
            );
        }
    }
}
