//! Typed data-model generation from a table's schema.
//!
//! Where `result_export_code` turns a result set's *rows* into a snippet,
//! this turns a table's *column metadata* (engine type, nullability, primary
//! key, identity, default) into a type definition in the target language:
//! TypeScript interface, Zod schema, Prisma model, Go struct, Python
//! dataclass, Java record, C# record, Rust struct, Kotlin data class, or
//! SQLAlchemy model. Everything is pure and GPUI-free so it can be tested
//! against fixture tables.
//!
//! The SQL type → category mapping and identifier casing live in
//! `codegen_types`, shared with the row exporter.

use sqlly_core::ResultColumn;
use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

use crate::codegen_types::{
    IdentCase, ParsedType, SqlTypeCategory as C, categorize_for, join_words, parse_type,
    sanitize_ident, type_name_for, unique_idents, words_of,
};

/// Target languages for model generation, in menu order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelLanguage {
    TypeScript,
    Zod,
    Prisma,
    Go,
    Python,
    Java,
    CSharp,
    Rust,
    Kotlin,
    SqlAlchemy,
}

impl ModelLanguage {
    pub fn all() -> &'static [ModelLanguage] {
        &[
            ModelLanguage::TypeScript,
            ModelLanguage::Zod,
            ModelLanguage::Prisma,
            ModelLanguage::Go,
            ModelLanguage::Python,
            ModelLanguage::Java,
            ModelLanguage::CSharp,
            ModelLanguage::Rust,
            ModelLanguage::Kotlin,
            ModelLanguage::SqlAlchemy,
        ]
    }

    /// Menu label naming the construct, e.g. "TypeScript interface".
    pub fn label(self) -> String {
        crate::localization::text(match self {
            ModelLanguage::TypeScript => "results.model_codegen.language_typescript",
            ModelLanguage::Zod => "results.model_codegen.language_zod",
            ModelLanguage::Prisma => "results.model_codegen.language_prisma",
            ModelLanguage::Go => "results.model_codegen.language_go",
            ModelLanguage::Python => "results.model_codegen.language_python",
            ModelLanguage::Java => "results.model_codegen.language_java",
            ModelLanguage::CSharp => "results.model_codegen.language_csharp",
            ModelLanguage::Rust => "results.model_codegen.language_rust",
            ModelLanguage::Kotlin => "results.model_codegen.language_kotlin",
            ModelLanguage::SqlAlchemy => "results.model_codegen.language_sqlalchemy",
        })
    }

    /// File extension for the generated source.
    pub fn extension(self) -> &'static str {
        match self {
            ModelLanguage::TypeScript | ModelLanguage::Zod => "ts",
            ModelLanguage::Prisma => "prisma",
            ModelLanguage::Go => "go",
            ModelLanguage::Python | ModelLanguage::SqlAlchemy => "py",
            ModelLanguage::Java => "java",
            ModelLanguage::CSharp => "cs",
            ModelLanguage::Rust => "rs",
            ModelLanguage::Kotlin => "kt",
        }
    }

    /// The row-exporter language this model language corresponds to, for
    /// sharing its enable/disable preference. Zod and Prisma ride
    /// TypeScript's switch; SQLAlchemy rides Python's.
    pub fn export_language(self) -> crate::result_export_code::CodeLanguage {
        use crate::result_export_code::CodeLanguage as L;
        match self {
            ModelLanguage::TypeScript | ModelLanguage::Zod | ModelLanguage::Prisma => L::TypeScript,
            ModelLanguage::Go => L::Go,
            ModelLanguage::Python | ModelLanguage::SqlAlchemy => L::Python,
            ModelLanguage::Java => L::Java,
            ModelLanguage::CSharp => L::CSharp,
            ModelLanguage::Rust => L::Rust,
            ModelLanguage::Kotlin => L::Kotlin,
        }
    }

    fn line_comment(self) -> &'static str {
        match self {
            ModelLanguage::Python | ModelLanguage::SqlAlchemy => "#",
            _ => "//",
        }
    }
}

/// One column's schema facts, as the generator needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelColumn {
    pub name: String,
    /// Engine type spelling (`nvarchar(50)`, `timestamp with time zone`).
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub identity: bool,
    /// Default expression as the catalog reports it (`((0))`, `getdate()`,
    /// `nextval('...')`).
    pub default: Option<String>,
}

/// A table (or view, or result set) to generate a model for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTable {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ModelColumn>,
}

impl ModelTable {
    /// From the structure-editor metadata (columns with defaults + PK).
    pub fn from_shape(shape: &sqlly_services::TableShape) -> Self {
        ModelTable {
            schema: shape.schema.clone(),
            name: shape.table.clone(),
            columns: shape
                .columns
                .iter()
                .map(|c| ModelColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    primary_key: shape.primary_key.iter().any(|k| k == &c.name),
                    identity: c.identity,
                    default: c.default.clone(),
                })
                .collect(),
        }
    }

    /// From a result set's column descriptors: no keys, identities, or
    /// defaults are known, so only names, types, and nullability carry over.
    pub fn from_result_columns(name: &str, columns: &[ResultColumn]) -> Self {
        ModelTable {
            schema: None,
            name: name.to_string(),
            columns: columns
                .iter()
                .map(|c| ModelColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    primary_key: false,
                    identity: false,
                    default: None,
                })
                .collect(),
        }
    }

    fn qualified(&self) -> String {
        match &self.schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Generate one table's model.
pub fn generate_model(language: ModelLanguage, db: DatabaseType, table: &ModelTable) -> String {
    generate_models(language, db, std::slice::from_ref(table))
}

/// Generate one source file holding a model for every table, with the
/// language's imports/preamble emitted once. Empty input yields an empty
/// string.
pub fn generate_models(language: ModelLanguage, db: DatabaseType, tables: &[ModelTable]) -> String {
    if tables.is_empty() {
        return String::new();
    }
    let case = match language {
        ModelLanguage::Python | ModelLanguage::SqlAlchemy | ModelLanguage::Rust => IdentCase::Snake,
        ModelLanguage::Go => IdentCase::Pascal,
        ModelLanguage::CSharp => IdentCase::Pascal,
        _ => IdentCase::Camel,
    };
    let resolved = resolve(tables, db, case, language == ModelLanguage::Go);
    let ctx = Ctx {
        db,
        multi: tables.len() > 1,
    };
    let mut out = String::new();
    let sources: Vec<String> = tables.iter().map(|t| t.qualified()).collect();
    let source = if ctx.multi {
        crate::localization::format(
            "results.model_codegen.multiple_table_source",
            &[("table_count", &tables.len().to_string())],
        )
    } else {
        sources[0].clone()
    };
    let heading = crate::localization::format(
        "results.model_codegen.generated_heading",
        &[
            ("source", &source),
            ("engine_name", &db.display_name().to_string()),
        ],
    );
    out.push_str(&format!("{} {heading}\n", language.line_comment()));
    match language {
        ModelLanguage::TypeScript => typescript(&ctx, &resolved, &mut out),
        ModelLanguage::Zod => zod(&ctx, &resolved, &mut out),
        ModelLanguage::Prisma => prisma(&ctx, &resolved, &mut out),
        ModelLanguage::Go => go(&ctx, &resolved, &mut out),
        ModelLanguage::Python => python(&ctx, &resolved, &mut out),
        ModelLanguage::Java => java(&ctx, &resolved, &mut out),
        ModelLanguage::CSharp => csharp(&ctx, &resolved, &mut out),
        ModelLanguage::Rust => rust(&ctx, &resolved, &mut out),
        ModelLanguage::Kotlin => kotlin(&ctx, &resolved, &mut out),
        ModelLanguage::SqlAlchemy => sqlalchemy(&ctx, &resolved, &mut out),
    }
    out
}

struct Ctx {
    db: DatabaseType,
    multi: bool,
}

struct Field<'a> {
    col: &'a ModelColumn,
    cat: C,
    parsed: ParsedType,
    ident: String,
}

impl Field<'_> {
    fn renamed(&self) -> bool {
        self.ident != self.col.name
    }

    /// Human notes for a trailing comment: original column name when the
    /// identifier differs, the engine type, and key/identity/default facts.
    fn notes(&self) -> String {
        self.notes_with(true, true)
    }

    /// `notes` minus the parts the language already states elsewhere: the
    /// column name when a tag/attribute carries it, the key/identity facts
    /// when attributes do.
    fn notes_with(&self, include_name: bool, include_keys: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        let ty = self.col.data_type.trim();
        if include_name && self.renamed() {
            if ty.is_empty() {
                parts.push(self.col.name.clone());
            } else {
                parts.push(format!("{}: {}", self.col.name, ty));
            }
        } else if !ty.is_empty() {
            parts.push(ty.to_string());
        }
        if include_keys && self.col.primary_key {
            parts.push("primary key".into());
        }
        if include_keys && self.col.identity {
            parts.push("identity".into());
        }
        if let Some(d) = self
            .col
            .default
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            parts.push(format!("default {d}"));
        }
        parts.join(", ")
    }
}

struct Model<'a> {
    table: &'a ModelTable,
    type_name: String,
    fields: Vec<Field<'a>>,
}

impl Model<'_> {
    fn pk_fields(&self) -> Vec<&Field<'_>> {
        self.fields.iter().filter(|f| f.col.primary_key).collect()
    }
    fn any(&self, pred: impl Fn(&Field<'_>) -> bool) -> bool {
        self.fields.iter().any(pred)
    }
}

/// Go initialisms: `id` → `ID`, `url` → `URL`, so `user_id` becomes `UserID`.
const GO_INITIALISMS: &[&str] = &[
    "id", "url", "uri", "uuid", "guid", "json", "xml", "sql", "html", "http", "https", "api", "ip",
    "ssn", "utc", "cpu", "ram", "os", "db",
];

fn go_ident(name: &str) -> String {
    let words = words_of(name);
    if words.is_empty() {
        return "Column".to_string();
    }
    let mut ident = String::new();
    for w in &words {
        let lower = w.to_lowercase();
        if GO_INITIALISMS.contains(&lower.as_str()) {
            ident.push_str(&lower.to_uppercase());
        } else {
            ident.push_str(&join_words(std::slice::from_ref(w), IdentCase::Pascal));
        }
    }
    if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("C{ident}")
    } else {
        ident
    }
}

fn resolve<'a>(
    tables: &'a [ModelTable],
    db: DatabaseType,
    case: IdentCase,
    go_style: bool,
) -> Vec<Model<'a>> {
    let type_names = unique_idents(tables.iter().map(|t| type_name_for(&t.name)).collect());
    tables
        .iter()
        .zip(type_names)
        .map(|(table, type_name)| {
            let idents = unique_idents(
                table
                    .columns
                    .iter()
                    .map(|c| {
                        if go_style {
                            go_ident(&c.name)
                        } else {
                            sanitize_ident(&c.name, case)
                        }
                    })
                    .collect(),
            );
            let fields = table
                .columns
                .iter()
                .zip(idents)
                .map(|(col, ident)| Field {
                    col,
                    cat: categorize_for(db, &col.data_type),
                    parsed: parse_type(&col.data_type),
                    ident,
                })
                .collect();
            Model {
                table,
                type_name,
                fields,
            }
        })
        .collect()
}

fn pad(s: &str, width: usize) -> String {
    format!("{s:<width$}")
}

fn trailing(comment: &str, notes: &str) -> String {
    if notes.is_empty() {
        String::new()
    } else {
        format!(" {comment} {notes}")
    }
}

/// Strip the SQL Server `((0))` / `('x')` wrapping and whitespace from a
/// default expression.
fn bare_default(expr: &str) -> String {
    let mut s = expr.trim();
    while wrapped_in_parens(s) {
        s = s[1..s.len() - 1].trim();
    }
    s.to_string()
}

/// True when `s` is `( … )` with the opening paren matched by the final one
/// (so `(a) + (b)` is left alone).
fn wrapped_in_parens(s: &str) -> bool {
    if s.len() < 2 || !s.starts_with('(') || !s.ends_with(')') {
        return false;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != s.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// A default expression as a literal: `Some(Number)`/`Some(Str)`/`Some(Bool)`
/// when it is one, `None` for functions and anything else.
enum DefaultLiteral {
    Number(String),
    Str(String),
    Bool(bool),
}

fn default_literal(expr: &str, cat: C) -> Option<DefaultLiteral> {
    let s = bare_default(expr);
    let lower = s.to_lowercase();
    if lower == "true" || lower == "b'1'" || (cat == C::Bool && lower == "1") {
        return Some(DefaultLiteral::Bool(true));
    }
    if lower == "false" || lower == "b'0'" || (cat == C::Bool && lower == "0") {
        return Some(DefaultLiteral::Bool(false));
    }
    if s.parse::<f64>().is_ok() {
        return Some(DefaultLiteral::Number(s));
    }
    let body = s
        .strip_prefix('N')
        .or_else(|| s.strip_prefix('n'))
        .unwrap_or(&s);
    if body.len() >= 2 && body.starts_with('\'') && body.ends_with('\'') {
        return Some(DefaultLiteral::Str(
            body[1..body.len() - 1].replace("''", "'"),
        ));
    }
    None
}

// ---------------------------------------------------------------- TypeScript

fn ts_type(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int | C::BigInt | C::Float | C::Decimal => "number",
        C::Bool => "boolean",
        C::DateTime | C::DateTimeOffset | C::Date => "Date",
        C::Binary => "Uint8Array",
        C::Json => "Record<string, unknown>",
        C::Time | C::Uuid | C::Text => "string",
    }
}

fn typescript(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    for m in models {
        out.push_str(&format!("export interface {} {{\n", m.type_name));
        for f in &m.fields {
            let ty = if f.col.nullable {
                format!("{} | null", ts_type(f))
            } else {
                ts_type(f).to_string()
            };
            out.push_str(&format!(
                "  {}: {};{}\n",
                f.ident,
                ty,
                trailing("//", &f.notes())
            ));
        }
        out.push_str("}\n");
        if models.len() > 1 {
            out.push('\n');
        }
    }
    if models.len() > 1 {
        out.pop();
    }
}

// ----------------------------------------------------------------------- Zod

fn zod_type(f: &Field<'_>) -> String {
    match f.cat {
        C::Int | C::BigInt => "z.number().int()".into(),
        C::Float | C::Decimal => "z.number()".into(),
        C::Bool => "z.boolean()".into(),
        C::DateTime | C::DateTimeOffset | C::Date => "z.date()".into(),
        C::Binary => "z.instanceof(Uint8Array)".into(),
        C::Json => "z.record(z.string(), z.unknown())".into(),
        C::Uuid => "z.string().uuid()".into(),
        C::Time => "z.string()".into(),
        C::Text => match f.parsed.length() {
            Some(n) if !f.parsed.is_max() => format!("z.string().max({n})"),
            _ => "z.string()".into(),
        },
    }
}

fn zod(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    out.push_str("import { z } from \"zod\";\n\n");
    for (i, m) in models.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "export const {}Schema = z.object({{\n",
            m.type_name
        ));
        for f in &m.fields {
            let mut ty = zod_type(f);
            if f.col.nullable {
                ty.push_str(".nullable()");
            }
            out.push_str(&format!(
                "  {}: {},{}\n",
                f.ident,
                ty,
                trailing("//", &f.notes())
            ));
        }
        out.push_str("});\n\n");
        out.push_str(&format!(
            "export type {} = z.infer<typeof {}Schema>;\n",
            m.type_name, m.type_name
        ));
    }
}

// -------------------------------------------------------------------- Prisma

fn prisma_scalar(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int => "Int",
        C::BigInt => "BigInt",
        C::Float => "Float",
        C::Decimal => "Decimal",
        C::Bool => "Boolean",
        C::DateTime | C::DateTimeOffset | C::Date | C::Time => "DateTime",
        C::Binary => "Bytes",
        C::Json => "Json",
        C::Uuid | C::Text => "String",
    }
}

/// `@db.*` native type attribute where Prisma has a matching one.
fn prisma_native(db: DatabaseType, f: &Field<'_>) -> Option<String> {
    let p = &f.parsed;
    let base = p.base.as_str();
    let len = || match p.length() {
        Some(n) => format!("({n})"),
        None if p.is_max() => "(Max)".to_string(),
        None => String::new(),
    };
    let len_req = || p.length().map(|n| format!("({n})"));
    let ps = || match p.precision_scale() {
        Some((pr, sc)) => format!("({pr},{sc})"),
        None => String::new(),
    };
    let name = match db.family() {
        Dialect::SqlServer => match base {
            "tinyint" => "TinyInt".into(),
            "smallint" => "SmallInt".into(),
            "int" => "Int".into(),
            "bigint" => "BigInt".into(),
            "bit" => "Bit".into(),
            "decimal" | "numeric" => format!("Decimal{}", ps()),
            "money" => "Money".into(),
            "smallmoney" => "SmallMoney".into(),
            "float" => "Float".into(),
            "real" => "Real".into(),
            "date" => "Date".into(),
            "time" => "Time".into(),
            "datetime" => "DateTime".into(),
            "datetime2" => "DateTime2".into(),
            "smalldatetime" => "SmallDateTime".into(),
            "datetimeoffset" => "DateTimeOffset".into(),
            "char" => format!("Char{}", len_req()?),
            "nchar" => format!("NChar{}", len_req()?),
            "varchar" => format!("VarChar{}", len()),
            "nvarchar" => format!("NVarChar{}", len()),
            "text" => "Text".into(),
            "ntext" => "NText".into(),
            "binary" => format!("Binary{}", len_req()?),
            "varbinary" => format!("VarBinary{}", len()),
            "image" => "Image".into(),
            "uniqueidentifier" => "UniqueIdentifier".into(),
            "xml" => "Xml".into(),
            _ => return None,
        },
        Dialect::Postgres => match base {
            "smallint" | "int2" | "smallserial" | "serial2" => "SmallInt".into(),
            "integer" | "int" | "int4" | "serial" | "serial4" => "Integer".into(),
            "bigint" | "int8" | "bigserial" | "serial8" => "BigInt".into(),
            "boolean" | "bool" => "Boolean".into(),
            "numeric" | "decimal" => format!("Decimal{}", ps()),
            "real" | "float4" => "Real".into(),
            "double precision" | "float8" => "DoublePrecision".into(),
            "money" => "Money".into(),
            "date" => "Date".into(),
            "time" | "time without time zone" => "Time".into(),
            "timetz" | "time with time zone" => "Timetz".into(),
            "timestamp" | "timestamp without time zone" => "Timestamp".into(),
            "timestamptz" | "timestamp with time zone" => "Timestamptz".into(),
            "char" | "character" | "bpchar" => format!("Char{}", len_req()?),
            "varchar" | "character varying" => format!("VarChar{}", len()),
            "text" => "Text".into(),
            "bytea" => "ByteA".into(),
            "json" => "Json".into(),
            "jsonb" => "JsonB".into(),
            "uuid" => "Uuid".into(),
            "xml" => "Xml".into(),
            "inet" => "Inet".into(),
            "citext" => "Citext".into(),
            _ => return None,
        },
        Dialect::MySql => {
            let u = if p.unsigned { "Unsigned" } else { "" };
            match base {
                "tinyint" => format!("{u}TinyInt"),
                "smallint" => format!("{u}SmallInt"),
                "mediumint" => format!("{u}MediumInt"),
                "int" | "integer" => format!("{u}Int"),
                "bigint" => format!("{u}BigInt"),
                "float" => "Float".into(),
                "double" => "Double".into(),
                "decimal" | "numeric" => format!("Decimal{}", ps()),
                "bit" => format!("Bit{}", len_req()?),
                "date" => "Date".into(),
                "time" => "Time".into(),
                "datetime" => "DateTime".into(),
                "timestamp" => "Timestamp".into(),
                "year" => "Year".into(),
                "char" => format!("Char{}", len_req()?),
                "varchar" => format!("VarChar{}", len_req()?),
                "tinytext" => "TinyText".into(),
                "text" => "Text".into(),
                "mediumtext" => "MediumText".into(),
                "longtext" => "LongText".into(),
                "binary" => format!("Binary{}", len_req()?),
                "varbinary" => format!("VarBinary{}", len_req()?),
                "tinyblob" => "TinyBlob".into(),
                "blob" => "Blob".into(),
                "mediumblob" => "MediumBlob".into(),
                "longblob" => "LongBlob".into(),
                "json" => "Json".into(),
                _ => return None,
            }
        }
        Dialect::Sqlite | Dialect::Oracle => return None,
    };
    Some(format!("@db.{name}"))
}

fn prisma_default(f: &Field<'_>) -> Option<String> {
    if f.col.identity {
        return Some("@default(autoincrement())".into());
    }
    let raw = f.col.default.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let bare = bare_default(raw);
    let lower = bare.to_lowercase().replace(' ', "");
    let value = match lower.as_str() {
        "getdate()"
        | "getutcdate()"
        | "sysdatetime()"
        | "sysutcdatetime()"
        | "now()"
        | "current_timestamp"
        | "current_timestamp()"
        | "localtimestamp"
        | "localtimestamp()"
        | "datetime('now')" => "now()".to_string(),
        "newid()" | "newsequentialid()" | "gen_random_uuid()" | "uuid_generate_v4()" | "uuid()" => {
            "uuid()".to_string()
        }
        _ => match default_literal(raw, f.cat) {
            Some(DefaultLiteral::Number(n)) => n,
            Some(DefaultLiteral::Bool(b)) => b.to_string(),
            Some(DefaultLiteral::Str(s)) => format!("\"{}\"", s.replace('"', "\\\"")),
            None => format!("dbgenerated(\"{}\")", bare.replace('"', "\\\"")),
        },
    };
    Some(format!("@default({value})"))
}

fn prisma(ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    for (i, m) in models.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("model {} {{\n", m.type_name));
        let pks = m.pk_fields();
        let single_pk = pks.len() == 1;
        let rows: Vec<(String, String, Vec<String>)> = m
            .fields
            .iter()
            .map(|f| {
                let mut ty = prisma_scalar(f).to_string();
                if f.col.nullable {
                    ty.push('?');
                }
                let mut attrs = Vec::new();
                if single_pk && f.col.primary_key {
                    attrs.push("@id".to_string());
                }
                if let Some(d) = prisma_default(f) {
                    attrs.push(d);
                }
                if f.renamed() {
                    attrs.push(format!("@map(\"{}\")", f.col.name.replace('"', "\\\"")));
                }
                if let Some(n) = prisma_native(ctx.db, f) {
                    attrs.push(n);
                }
                (f.ident.clone(), ty, attrs)
            })
            .collect();
        let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
        let type_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        for (name, ty, attrs) in &rows {
            let mut line = format!("  {} {}", pad(name, name_w), pad(ty, type_w));
            if !attrs.is_empty() {
                line.push(' ');
                line.push_str(&attrs.join(" "));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        let mut block = Vec::new();
        if pks.len() > 1 {
            block.push(format!(
                "@@id([{}])",
                pks.iter()
                    .map(|f| f.ident.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if m.type_name != m.table.name {
            block.push(format!("@@map(\"{}\")", m.table.name.replace('"', "\\\"")));
        }
        if !block.is_empty() {
            out.push('\n');
            for b in block {
                out.push_str(&format!("  {b}\n"));
            }
        }
        out.push_str("}\n");
    }
}

// ------------------------------------------------------------------------ Go

fn go_type(f: &Field<'_>) -> String {
    let nullable = f.col.nullable;
    match f.cat {
        C::Int => if nullable { "sql.NullInt32" } else { "int32" }.into(),
        C::BigInt => if nullable { "sql.NullInt64" } else { "int64" }.into(),
        C::Float | C::Decimal => if nullable {
            "sql.NullFloat64"
        } else {
            "float64"
        }
        .into(),
        C::Bool => if nullable { "sql.NullBool" } else { "bool" }.into(),
        C::DateTime | C::DateTimeOffset | C::Date | C::Time => if nullable {
            "sql.NullTime"
        } else {
            "time.Time"
        }
        .into(),
        C::Binary => "[]byte".into(),
        C::Json => "json.RawMessage".into(),
        C::Uuid | C::Text => if nullable { "sql.NullString" } else { "string" }.into(),
    }
}

fn go(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    out.push_str("package models\n");
    let mut imports = Vec::new();
    let any = |pred: &dyn Fn(&Field<'_>) -> bool| models.iter().any(|m| m.any(pred));
    if any(&|f| f.col.nullable && !matches!(f.cat, C::Binary | C::Json)) {
        imports.push("\"database/sql\"");
    }
    if any(&|f| f.cat == C::Json) {
        imports.push("\"encoding/json\"");
    }
    if any(&|f| f.cat.is_temporal() && !f.col.nullable) {
        imports.push("\"time\"");
    }
    if !imports.is_empty() {
        out.push_str("\nimport (\n");
        for i in imports {
            out.push_str(&format!("\t{i}\n"));
        }
        out.push_str(")\n");
    }
    for m in models {
        out.push_str(&format!(
            "\n// {} maps {}.\ntype {} struct {{\n",
            m.type_name,
            m.table.qualified(),
            m.type_name
        ));
        let rows: Vec<(String, String, String, String)> = m
            .fields
            .iter()
            .map(|f| {
                let col = f.col.name.replace('"', "\\\"");
                (
                    f.ident.clone(),
                    go_type(f),
                    format!("`json:\"{col}\" db:\"{col}\"`"),
                    f.notes_with(false, true),
                )
            })
            .collect();
        let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
        let type_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        let tag_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0);
        for (name, ty, tag, notes) in &rows {
            let line = if notes.is_empty() {
                format!("\t{} {} {}", pad(name, name_w), pad(ty, type_w), tag)
            } else {
                format!(
                    "\t{} {} {} // {}",
                    pad(name, name_w),
                    pad(ty, type_w),
                    pad(tag, tag_w),
                    notes
                )
            };
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push_str("}\n");
    }
}

// -------------------------------------------------------------------- Python

fn py_type(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int | C::BigInt => "int",
        C::Float => "float",
        C::Decimal => "Decimal",
        C::Bool => "bool",
        C::DateTime | C::DateTimeOffset => "datetime",
        C::Date => "date",
        C::Time => "time",
        C::Binary => "bytes",
        C::Json => "Any",
        C::Uuid => "UUID",
        C::Text => "str",
    }
}

/// The `from … import …` lines the Python types in `models` need, sorted.
fn py_imports(models: &[Model<'_>]) -> Vec<String> {
    let any = |pred: &dyn Fn(&Field<'_>) -> bool| models.iter().any(|m| m.any(pred));
    let mut lines = Vec::new();
    let mut dt: Vec<&str> = Vec::new();
    if any(&|f| f.cat == C::Date) {
        dt.push("date");
    }
    if any(&|f| matches!(f.cat, C::DateTime | C::DateTimeOffset)) {
        dt.push("datetime");
    }
    if any(&|f| f.cat == C::Time) {
        dt.push("time");
    }
    if !dt.is_empty() {
        lines.push(format!("from datetime import {}", dt.join(", ")));
    }
    if any(&|f| f.cat == C::Decimal) {
        lines.push("from decimal import Decimal".into());
    }
    let mut typing: Vec<&str> = Vec::new();
    if any(&|f| f.cat == C::Json) {
        typing.push("Any");
    }
    if any(&|f| f.col.nullable) {
        typing.push("Optional");
    }
    if !typing.is_empty() {
        lines.push(format!("from typing import {}", typing.join(", ")));
    }
    if any(&|f| f.cat == C::Uuid) {
        lines.push("from uuid import UUID".into());
    }
    lines
}

fn python(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    out.push_str("from dataclasses import dataclass\n");
    for line in py_imports(models) {
        out.push_str(&line);
        out.push('\n');
    }
    for m in models {
        out.push_str(&format!(
            "\n\n@dataclass\nclass {}:\n    \"\"\"Maps {}.\"\"\"\n\n",
            m.type_name,
            m.table.qualified()
        ));
        if m.fields.is_empty() {
            out.push_str("    pass\n");
        }
        for f in &m.fields {
            let ty = if f.col.nullable {
                format!("Optional[{}]", py_type(f))
            } else {
                py_type(f).to_string()
            };
            out.push_str(&format!(
                "    {}: {}{}\n",
                f.ident,
                ty,
                trailing(" #", &f.notes())
            ));
        }
    }
}

// ---------------------------------------------------------------------- Java

fn java_type(f: &Field<'_>) -> &'static str {
    let n = f.col.nullable;
    match f.cat {
        C::Int => {
            if n {
                "Integer"
            } else {
                "int"
            }
        }
        C::BigInt => {
            if n {
                "Long"
            } else {
                "long"
            }
        }
        C::Float => {
            if n {
                "Double"
            } else {
                "double"
            }
        }
        C::Decimal => "BigDecimal",
        C::Bool => {
            if n {
                "Boolean"
            } else {
                "boolean"
            }
        }
        C::DateTime => "LocalDateTime",
        C::DateTimeOffset => "OffsetDateTime",
        C::Date => "LocalDate",
        C::Time => "LocalTime",
        C::Binary => "byte[]",
        C::Uuid => "UUID",
        C::Json | C::Text => "String",
    }
}

fn jvm_imports(models: &[Model<'_>], semicolon: bool) -> Vec<String> {
    let any = |pred: &dyn Fn(&Field<'_>) -> bool| models.iter().any(|m| m.any(pred));
    let mut lines: Vec<String> = Vec::new();
    if any(&|f| f.cat == C::Decimal) {
        lines.push("import java.math.BigDecimal".into());
    }
    if any(&|f| f.cat == C::Date) {
        lines.push("import java.time.LocalDate".into());
    }
    if any(&|f| f.cat == C::DateTime) {
        lines.push("import java.time.LocalDateTime".into());
    }
    if any(&|f| f.cat == C::Time) {
        lines.push("import java.time.LocalTime".into());
    }
    if any(&|f| f.cat == C::DateTimeOffset) {
        lines.push("import java.time.OffsetDateTime".into());
    }
    if any(&|f| f.cat == C::Uuid) {
        lines.push("import java.util.UUID".into());
    }
    if semicolon {
        for l in &mut lines {
            l.push(';');
        }
    }
    lines
}

fn java(ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    let imports = jvm_imports(models, true);
    if !imports.is_empty() {
        out.push('\n');
        for l in imports {
            out.push_str(&l);
            out.push('\n');
        }
    }
    if ctx.multi {
        out.push_str(
            "\n// One file, several records: only one may be public, so these are package-private.\n",
        );
    }
    for m in models {
        out.push_str(&format!("\n/** Maps {}. */\n", m.table.qualified()));
        let vis = if ctx.multi { "" } else { "public " };
        out.push_str(&format!("{vis}record {}(", m.type_name));
        if m.fields.is_empty() {
            out.push_str(") {}\n");
            continue;
        }
        out.push('\n');
        let rows: Vec<(String, String)> = m
            .fields
            .iter()
            .map(|f| (format!("{} {}", java_type(f), f.ident), f.notes()))
            .collect();
        let decl_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0) + 1;
        let last = rows.len() - 1;
        for (i, (decl, notes)) in rows.iter().enumerate() {
            let decl = if i == last {
                decl.clone()
            } else {
                format!("{decl},")
            };
            let line = if notes.is_empty() {
                format!("    {decl}")
            } else {
                format!("    {} // {}", pad(&decl, decl_w), notes)
            };
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push_str(") {}\n");
    }
}

// ------------------------------------------------------------------------ C#

fn cs_type(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int => "int",
        C::BigInt => "long",
        C::Float => "double",
        C::Decimal => "decimal",
        C::Bool => "bool",
        C::DateTime => "DateTime",
        C::DateTimeOffset => "DateTimeOffset",
        C::Date => "DateOnly",
        C::Time => "TimeOnly",
        C::Binary => "byte[]",
        C::Uuid => "Guid",
        C::Json | C::Text => "string",
    }
}

fn csharp(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    out.push_str(
        "using System;\nusing System.ComponentModel.DataAnnotations;\nusing System.ComponentModel.DataAnnotations.Schema;\n",
    );
    for m in models {
        out.push('\n');
        let table = m.table.name.replace('"', "\\\"");
        match m.table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(schema) => out.push_str(&format!(
                "[Table(\"{table}\", Schema = \"{}\")]\n",
                schema.replace('"', "\\\"")
            )),
            None => out.push_str(&format!("[Table(\"{table}\")]\n")),
        }
        out.push_str(&format!("public record {}\n{{\n", m.type_name));
        for (i, f) in m.fields.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if f.col.primary_key {
                out.push_str("    [Key]\n");
            }
            if f.col.identity {
                out.push_str("    [DatabaseGenerated(DatabaseGeneratedOption.Identity)]\n");
            }
            out.push_str(&format!(
                "    [Column(\"{}\")]\n",
                f.col.name.replace('"', "\\\"")
            ));
            let ty = if f.col.nullable {
                format!("{}?", cs_type(f))
            } else {
                cs_type(f).to_string()
            };
            let ty_notes = f.notes_with(false, false);
            out.push_str(&format!(
                "    public {} {} {{ get; init; }}{}\n",
                ty,
                f.ident,
                trailing("//", &ty_notes)
            ));
        }
        out.push_str("}\n");
    }
}

// ---------------------------------------------------------------------- Rust

fn rust_type(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int => "i32",
        C::BigInt => "i64",
        C::Float => "f64",
        C::Decimal => "rust_decimal::Decimal",
        C::Bool => "bool",
        C::DateTime => "chrono::NaiveDateTime",
        C::DateTimeOffset => "chrono::DateTime<chrono::Utc>",
        C::Date => "chrono::NaiveDate",
        C::Time => "chrono::NaiveTime",
        C::Binary => "Vec<u8>",
        C::Json => "serde_json::Value",
        C::Uuid => "uuid::Uuid",
        C::Text => "String",
    }
}

fn rust(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    out.push_str("use serde::{Deserialize, Serialize};\n");
    for m in models {
        out.push_str(&format!(
            "\n/// Maps {}.\n#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct {} {{\n",
            m.table.qualified(),
            m.type_name
        ));
        for f in &m.fields {
            let notes = f.notes();
            if !notes.is_empty() {
                out.push_str(&format!("    /// {notes}\n"));
            }
            if f.renamed() {
                out.push_str(&format!(
                    "    #[serde(rename = \"{}\")]\n",
                    f.col.name.replace('"', "\\\"")
                ));
            }
            let ty = if f.col.nullable {
                format!("Option<{}>", rust_type(f))
            } else {
                rust_type(f).to_string()
            };
            out.push_str(&format!("    pub {}: {},\n", f.ident, ty));
        }
        out.push_str("}\n");
    }
}

// -------------------------------------------------------------------- Kotlin

fn kotlin_type(f: &Field<'_>) -> &'static str {
    match f.cat {
        C::Int => "Int",
        C::BigInt => "Long",
        C::Float => "Double",
        C::Decimal => "BigDecimal",
        C::Bool => "Boolean",
        C::DateTime => "LocalDateTime",
        C::DateTimeOffset => "OffsetDateTime",
        C::Date => "LocalDate",
        C::Time => "LocalTime",
        C::Binary => "ByteArray",
        C::Uuid => "UUID",
        C::Json | C::Text => "String",
    }
}

fn kotlin(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    let imports = jvm_imports(models, false);
    if !imports.is_empty() {
        out.push('\n');
        for l in imports {
            out.push_str(&l);
            out.push('\n');
        }
    }
    for m in models {
        out.push_str(&format!(
            "\n/** Maps {}. */\ndata class {}(\n",
            m.table.qualified(),
            m.type_name
        ));
        let rows: Vec<(String, String)> = m
            .fields
            .iter()
            .map(|f| {
                let ty = if f.col.nullable {
                    format!("{}?", kotlin_type(f))
                } else {
                    kotlin_type(f).to_string()
                };
                (format!("val {}: {},", f.ident, ty), f.notes())
            })
            .collect();
        let decl_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
        for (decl, notes) in &rows {
            let line = if notes.is_empty() {
                format!("    {decl}")
            } else {
                format!("    {} // {}", pad(decl, decl_w), notes)
            };
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push_str(")\n");
    }
}

// ---------------------------------------------------------------- SQLAlchemy

/// `(sqlalchemy type expression, name to import)`.
fn sa_type(f: &Field<'_>) -> (String, &'static str) {
    match f.cat {
        C::Int => ("Integer".into(), "Integer"),
        C::BigInt => ("BigInteger".into(), "BigInteger"),
        C::Float => ("Float".into(), "Float"),
        C::Decimal => match f.parsed.precision_scale() {
            Some((p, s)) => (format!("Numeric({p}, {s})"), "Numeric"),
            None => ("Numeric".into(), "Numeric"),
        },
        C::Bool => ("Boolean".into(), "Boolean"),
        C::DateTime => ("DateTime".into(), "DateTime"),
        C::DateTimeOffset => ("DateTime(timezone=True)".into(), "DateTime"),
        C::Date => ("Date".into(), "Date"),
        C::Time => ("Time".into(), "Time"),
        C::Binary => ("LargeBinary".into(), "LargeBinary"),
        C::Json => ("JSON".into(), "JSON"),
        C::Uuid => ("Uuid".into(), "Uuid"),
        C::Text => match f.parsed.length() {
            Some(n) if !f.parsed.is_max() => (format!("String({n})"), "String"),
            _ if f.parsed.base.contains("text") || f.parsed.is_max() => ("Text".into(), "Text"),
            _ => ("String".into(), "String"),
        },
    }
}

fn sqlalchemy(_ctx: &Ctx, models: &[Model<'_>], out: &mut String) {
    let any = |pred: &dyn Fn(&Field<'_>) -> bool| models.iter().any(|m| m.any(pred));
    let std_imports = py_imports(models);
    if !std_imports.is_empty() {
        for line in &std_imports {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    let mut sa_names: Vec<&str> = models
        .iter()
        .flat_map(|m| m.fields.iter().map(|f| sa_type(f).1))
        .collect();
    let needs_text = any(&|f| {
        !f.col.identity
            && f.col
                .default
                .as_deref()
                .map(str::trim)
                .is_some_and(|d| !d.is_empty())
    });
    if needs_text {
        sa_names.push("text");
    }
    sa_names.sort_unstable();
    sa_names.dedup();
    if !sa_names.is_empty() {
        out.push_str(&format!("from sqlalchemy import {}\n", sa_names.join(", ")));
    }
    out.push_str("from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\n");
    out.push_str("\n\nclass Base(DeclarativeBase):\n    pass\n");
    for m in models {
        out.push_str(&format!(
            "\n\nclass {}(Base):\n    __tablename__ = \"{}\"\n",
            m.type_name,
            m.table.name.replace('"', "\\\"")
        ));
        if let Some(schema) = m.table.schema.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!(
                "    __table_args__ = {{\"schema\": \"{}\"}}\n",
                schema.replace('"', "\\\"")
            ));
        }
        let has_pk = m.fields.iter().any(|f| f.col.primary_key);
        if !m.fields.is_empty() && !has_pk {
            out.push_str(
                "    # TODO: the table declares no primary key; SQLAlchemy needs one, so the first column is used.\n",
            );
        }
        out.push('\n');
        for (i, f) in m.fields.iter().enumerate() {
            let (sa, _) = sa_type(f);
            let py = py_type(f);
            // `Base.metadata` / `Base.registry` are SQLAlchemy's own; a column
            // by that name keeps its real name in `mapped_column`.
            let ident = if f.ident == "metadata" || f.ident == "registry" {
                format!("{}_", f.ident)
            } else {
                f.ident.clone()
            };
            let mapped = if f.col.nullable {
                format!("Mapped[Optional[{py}]]")
            } else {
                format!("Mapped[{py}]")
            };
            let mut args = vec![format!("\"{}\"", f.col.name.replace('"', "\\\"")), sa];
            if f.col.primary_key || (!has_pk && i == 0) {
                args.push("primary_key=True".into());
            }
            if f.col.identity {
                args.push("autoincrement=True".into());
            } else if let Some(d) = f
                .col
                .default
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                args.push(format!(
                    "server_default=text(\"{}\")",
                    d.replace('\\', "\\\\").replace('"', "\\\"")
                ));
            }
            out.push_str(&format!(
                "    {}: {} = mapped_column({})\n",
                ident,
                mapped,
                args.join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(
        name: &str,
        data_type: &str,
        nullable: bool,
        primary_key: bool,
        identity: bool,
        default: Option<&str>,
    ) -> ModelColumn {
        ModelColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            primary_key,
            identity,
            default: default.map(str::to_string),
        }
    }

    /// The fixture table every golden covers: identity PK, nullable text
    /// with a length, decimal, datetime with a default, binary, bool, json,
    /// uuid, bigint, and a reserved-word column.
    fn fixture() -> ModelTable {
        ModelTable {
            schema: Some("dbo".into()),
            name: "Orders".into(),
            columns: vec![
                col("id", "int", false, true, true, None),
                col("customer_name", "nvarchar(100)", true, false, false, None),
                col("total", "decimal(10,2)", false, false, false, Some("((0))")),
                col(
                    "created_at",
                    "datetime2(7)",
                    false,
                    false,
                    false,
                    Some("(getdate())"),
                ),
                col("signature", "varbinary(max)", true, false, false, None),
                col("is_paid", "bit", false, false, false, Some("((0))")),
                col("metadata", "json", true, false, false, None),
                col(
                    "public_id",
                    "uniqueidentifier",
                    false,
                    false,
                    false,
                    Some("(newid())"),
                ),
                col("row_count", "bigint", true, false, false, None),
                col("type", "varchar(20)", false, false, false, None),
            ],
        }
    }

    fn norm(s: &str) -> String {
        s.replace("\r\n", "\n")
    }

    fn assert_golden(language: ModelLanguage, expected: &str) {
        let actual = generate_model(language, DatabaseType::SqlServer, &fixture());
        assert_eq!(
            norm(&actual),
            norm(expected),
            "{} output:\n{}",
            language.label(),
            actual
        );
    }

    #[test]
    fn languages_have_labels_and_extensions() {
        assert_eq!(ModelLanguage::all().len(), 10);
        for l in ModelLanguage::all() {
            assert!(!l.label().is_empty());
            assert!(!l.extension().is_empty());
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        for l in ModelLanguage::all() {
            assert_eq!(generate_models(*l, DatabaseType::Postgres, &[]), "");
        }
    }

    #[test]
    fn typescript_golden() {
        assert_golden(
            ModelLanguage::TypeScript,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
export interface Order {
  id: number; // int, primary key, identity
  customerName: string | null; // customer_name: nvarchar(100)
  total: number; // decimal(10,2), default ((0))
  createdAt: Date; // created_at: datetime2(7), default (getdate())
  signature: Uint8Array | null; // varbinary(max)
  isPaid: boolean; // is_paid: bit, default ((0))
  metadata: Record<string, unknown> | null; // json
  publicId: string; // public_id: uniqueidentifier, default (newid())
  rowCount: number | null; // row_count: bigint
  type_: string; // type: varchar(20)
}
"#,
        );
    }

    #[test]
    fn zod_golden() {
        assert_golden(
            ModelLanguage::Zod,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
import { z } from "zod";

export const OrderSchema = z.object({
  id: z.number().int(), // int, primary key, identity
  customerName: z.string().max(100).nullable(), // customer_name: nvarchar(100)
  total: z.number(), // decimal(10,2), default ((0))
  createdAt: z.date(), // created_at: datetime2(7), default (getdate())
  signature: z.instanceof(Uint8Array).nullable(), // varbinary(max)
  isPaid: z.boolean(), // is_paid: bit, default ((0))
  metadata: z.record(z.string(), z.unknown()).nullable(), // json
  publicId: z.string().uuid(), // public_id: uniqueidentifier, default (newid())
  rowCount: z.number().int().nullable(), // row_count: bigint
  type_: z.string().max(20), // type: varchar(20)
});

export type Order = z.infer<typeof OrderSchema>;
"#,
        );
    }

    #[test]
    fn prisma_golden() {
        assert_golden(
            ModelLanguage::Prisma,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
model Order {
  id           Int      @id @default(autoincrement()) @db.Int
  customerName String?  @map("customer_name") @db.NVarChar(100)
  total        Decimal  @default(0) @db.Decimal(10,2)
  createdAt    DateTime @default(now()) @map("created_at") @db.DateTime2
  signature    Bytes?   @db.VarBinary(Max)
  isPaid       Boolean  @default(false) @map("is_paid") @db.Bit
  metadata     Json?
  publicId     String   @default(uuid()) @map("public_id") @db.UniqueIdentifier
  rowCount     BigInt?  @map("row_count") @db.BigInt
  type_        String   @map("type") @db.VarChar(20)

  @@map("Orders")
}
"#,
        );
    }

    #[test]
    fn go_golden() {
        assert_golden(
            ModelLanguage::Go,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
package models

import (
	"database/sql"
	"encoding/json"
	"time"
)

// Order maps dbo.Orders.
type Order struct {
	ID           int32           `json:"id" db:"id"`                       // int, primary key, identity
	CustomerName sql.NullString  `json:"customer_name" db:"customer_name"` // nvarchar(100)
	Total        float64         `json:"total" db:"total"`                 // decimal(10,2), default ((0))
	CreatedAt    time.Time       `json:"created_at" db:"created_at"`       // datetime2(7), default (getdate())
	Signature    []byte          `json:"signature" db:"signature"`         // varbinary(max)
	IsPaid       bool            `json:"is_paid" db:"is_paid"`             // bit, default ((0))
	Metadata     json.RawMessage `json:"metadata" db:"metadata"`           // json
	PublicID     string          `json:"public_id" db:"public_id"`         // uniqueidentifier, default (newid())
	RowCount     sql.NullInt64   `json:"row_count" db:"row_count"`         // bigint
	Type         string          `json:"type" db:"type"`                   // varchar(20)
}
"#,
        );
    }

    #[test]
    fn python_golden() {
        assert_golden(
            ModelLanguage::Python,
            r#"# Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
from dataclasses import dataclass
from datetime import datetime
from decimal import Decimal
from typing import Any, Optional
from uuid import UUID


@dataclass
class Order:
    """Maps dbo.Orders."""

    id: int  # int, primary key, identity
    customer_name: Optional[str]  # nvarchar(100)
    total: Decimal  # decimal(10,2), default ((0))
    created_at: datetime  # datetime2(7), default (getdate())
    signature: Optional[bytes]  # varbinary(max)
    is_paid: bool  # bit, default ((0))
    metadata: Optional[Any]  # json
    public_id: UUID  # uniqueidentifier, default (newid())
    row_count: Optional[int]  # bigint
    type_: str  # type: varchar(20)
"#,
        );
    }

    #[test]
    fn java_golden() {
        assert_golden(
            ModelLanguage::Java,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)

import java.math.BigDecimal;
import java.time.LocalDateTime;
import java.util.UUID;

/** Maps dbo.Orders. */
public record Order(
    int id,                  // int, primary key, identity
    String customerName,     // customer_name: nvarchar(100)
    BigDecimal total,        // decimal(10,2), default ((0))
    LocalDateTime createdAt, // created_at: datetime2(7), default (getdate())
    byte[] signature,        // varbinary(max)
    boolean isPaid,          // is_paid: bit, default ((0))
    String metadata,         // json
    UUID publicId,           // public_id: uniqueidentifier, default (newid())
    Long rowCount,           // row_count: bigint
    String type_             // type: varchar(20)
) {}
"#,
        );
    }

    #[test]
    fn csharp_golden() {
        assert_golden(
            ModelLanguage::CSharp,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
using System;
using System.ComponentModel.DataAnnotations;
using System.ComponentModel.DataAnnotations.Schema;

[Table("Orders", Schema = "dbo")]
public record Order
{
    [Key]
    [DatabaseGenerated(DatabaseGeneratedOption.Identity)]
    [Column("id")]
    public int Id { get; init; } // int

    [Column("customer_name")]
    public string? CustomerName { get; init; } // nvarchar(100)

    [Column("total")]
    public decimal Total { get; init; } // decimal(10,2), default ((0))

    [Column("created_at")]
    public DateTime CreatedAt { get; init; } // datetime2(7), default (getdate())

    [Column("signature")]
    public byte[]? Signature { get; init; } // varbinary(max)

    [Column("is_paid")]
    public bool IsPaid { get; init; } // bit, default ((0))

    [Column("metadata")]
    public string? Metadata { get; init; } // json

    [Column("public_id")]
    public Guid PublicId { get; init; } // uniqueidentifier, default (newid())

    [Column("row_count")]
    public long? RowCount { get; init; } // bigint

    [Column("type")]
    public string Type_ { get; init; } // varchar(20)
}
"#,
        );
    }

    #[test]
    fn rust_golden() {
        assert_golden(
            ModelLanguage::Rust,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
use serde::{Deserialize, Serialize};

/// Maps dbo.Orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// int, primary key, identity
    pub id: i32,
    /// nvarchar(100)
    pub customer_name: Option<String>,
    /// decimal(10,2), default ((0))
    pub total: rust_decimal::Decimal,
    /// datetime2(7), default (getdate())
    pub created_at: chrono::NaiveDateTime,
    /// varbinary(max)
    pub signature: Option<Vec<u8>>,
    /// bit, default ((0))
    pub is_paid: bool,
    /// json
    pub metadata: Option<serde_json::Value>,
    /// uniqueidentifier, default (newid())
    pub public_id: uuid::Uuid,
    /// bigint
    pub row_count: Option<i64>,
    /// type: varchar(20)
    #[serde(rename = "type")]
    pub type_: String,
}
"#,
        );
    }

    #[test]
    fn kotlin_golden() {
        assert_golden(
            ModelLanguage::Kotlin,
            r#"// Generated by SQLLY from dbo.Orders (Microsoft SQL Server)

import java.math.BigDecimal
import java.time.LocalDateTime
import java.util.UUID

/** Maps dbo.Orders. */
data class Order(
    val id: Int,                  // int, primary key, identity
    val customerName: String?,    // customer_name: nvarchar(100)
    val total: BigDecimal,        // decimal(10,2), default ((0))
    val createdAt: LocalDateTime, // created_at: datetime2(7), default (getdate())
    val signature: ByteArray?,    // varbinary(max)
    val isPaid: Boolean,          // is_paid: bit, default ((0))
    val metadata: String?,        // json
    val publicId: UUID,           // public_id: uniqueidentifier, default (newid())
    val rowCount: Long?,          // row_count: bigint
    val type_: String,            // type: varchar(20)
)
"#,
        );
    }

    #[test]
    fn sqlalchemy_golden() {
        assert_golden(
            ModelLanguage::SqlAlchemy,
            r#"# Generated by SQLLY from dbo.Orders (Microsoft SQL Server)
from datetime import datetime
from decimal import Decimal
from typing import Any, Optional
from uuid import UUID

from sqlalchemy import BigInteger, Boolean, DateTime, Integer, JSON, LargeBinary, Numeric, String, Uuid, text
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class Order(Base):
    __tablename__ = "Orders"
    __table_args__ = {"schema": "dbo"}

    id: Mapped[int] = mapped_column("id", Integer, primary_key=True, autoincrement=True)
    customer_name: Mapped[Optional[str]] = mapped_column("customer_name", String(100))
    total: Mapped[Decimal] = mapped_column("total", Numeric(10, 2), server_default=text("((0))"))
    created_at: Mapped[datetime] = mapped_column("created_at", DateTime, server_default=text("(getdate())"))
    signature: Mapped[Optional[bytes]] = mapped_column("signature", LargeBinary)
    is_paid: Mapped[bool] = mapped_column("is_paid", Boolean, server_default=text("((0))"))
    metadata_: Mapped[Optional[Any]] = mapped_column("metadata", JSON)
    public_id: Mapped[UUID] = mapped_column("public_id", Uuid, server_default=text("(newid())"))
    row_count: Mapped[Optional[int]] = mapped_column("row_count", BigInteger)
    type_: Mapped[str] = mapped_column("type", String(20))
"#,
        );
    }

    #[test]
    fn prisma_uses_engine_native_types_and_composite_ids() {
        let table = ModelTable {
            schema: Some("public".into()),
            name: "order_items".into(),
            columns: vec![
                col("order_id", "integer", false, true, false, None),
                col("line_no", "smallint", false, true, false, None),
                col("qty", "numeric(8,3)", false, false, false, Some("1")),
                col("note", "character varying(40)", true, false, false, None),
                col(
                    "added",
                    "timestamp with time zone",
                    false,
                    false,
                    false,
                    Some("now()"),
                ),
                col("payload", "jsonb", true, false, false, None),
            ],
        };
        let out = generate_model(ModelLanguage::Prisma, DatabaseType::Postgres, &table);
        assert!(out.contains("model OrderItem {"), "{out}");
        assert!(out.contains("@@id([orderId, lineNo])"), "{out}");
        assert!(!out.contains("@id "), "{out}");
        assert!(out.contains("@db.Integer"), "{out}");
        assert!(out.contains("@db.SmallInt"), "{out}");
        assert!(out.contains("@default(1) @db.Decimal(8,3)"), "{out}");
        assert!(out.contains("@db.VarChar(40)"), "{out}");
        assert!(out.contains("@default(now()) @db.Timestamptz"), "{out}");
        assert!(out.contains("Json?"), "{out}");
        assert!(out.contains("@db.JsonB"), "{out}");
        assert!(out.contains("@@map(\"order_items\")"), "{out}");
    }

    #[test]
    fn prisma_mysql_unsigned_and_dbgenerated_defaults() {
        let table = ModelTable {
            schema: None,
            name: "users".into(),
            columns: vec![
                col("id", "int unsigned", false, true, true, None),
                col("flag", "tinyint(1)", false, false, false, Some("b'1'")),
                col(
                    "code",
                    "varchar(8)",
                    false,
                    false,
                    false,
                    Some("upper(uuid())"),
                ),
                col("body", "longtext", true, false, false, None),
            ],
        };
        let out = generate_model(ModelLanguage::Prisma, DatabaseType::MySql, &table);
        assert!(
            out.contains("@id @default(autoincrement()) @db.UnsignedInt"),
            "{out}"
        );
        assert!(out.contains("Boolean @default(true) @db.TinyInt"), "{out}");
        assert!(
            out.contains("@default(dbgenerated(\"upper(uuid())\")) @db.VarChar(8)"),
            "{out}"
        );
        assert!(out.contains("@db.LongText"), "{out}");
    }

    #[test]
    fn prisma_sqlite_has_no_native_types_and_maps_pascal_table() {
        let table = ModelTable {
            schema: None,
            name: "Customer".into(),
            columns: vec![
                col("Id", "INTEGER", false, true, false, None),
                col("Name", "TEXT", false, false, false, None),
            ],
        };
        let out = generate_model(ModelLanguage::Prisma, DatabaseType::Sqlite, &table);
        assert!(!out.contains("@db."), "{out}");
        assert!(!out.contains("@@map"), "{out}");
        assert!(out.contains("id   Int    @id @map(\"Id\")"), "{out}");
    }

    #[test]
    fn go_nullable_uses_sql_null_types_and_initialisms() {
        let table = ModelTable {
            schema: None,
            name: "sessions".into(),
            columns: vec![
                col("user_id", "bigint", false, true, false, None),
                col("ip_address", "varchar(45)", true, false, false, None),
                col("expires", "timestamp", true, false, false, None),
                col("active", "boolean", true, false, false, None),
            ],
        };
        let out = generate_model(ModelLanguage::Go, DatabaseType::Postgres, &table);
        assert!(out.contains("UserID    int64"), "{out}");
        assert!(out.contains("IPAddress sql.NullString"), "{out}");
        assert!(out.contains("Expires   sql.NullTime"), "{out}");
        assert!(out.contains("Active    sql.NullBool"), "{out}");
        assert!(out.contains("\"database/sql\""), "{out}");
        assert!(!out.contains("\"time\""), "{out}");
    }

    #[test]
    fn python_imports_only_what_the_columns_need() {
        let table = ModelTable {
            schema: None,
            name: "tags".into(),
            columns: vec![
                col("id", "int", false, true, false, None),
                col("label", "text", false, false, false, None),
            ],
        };
        let out = generate_model(ModelLanguage::Python, DatabaseType::Sqlite, &table);
        assert!(!out.contains("from typing"), "{out}");
        assert!(!out.contains("from datetime"), "{out}");
        assert!(out.contains("class Tag:"), "{out}");
        assert!(out.contains("    id: int  # int, primary key\n"), "{out}");
    }

    #[test]
    fn multi_table_files_share_one_preamble() {
        let a = ModelTable {
            schema: Some("dbo".into()),
            name: "Users".into(),
            columns: vec![col("id", "int", false, true, true, None)],
        };
        let b = ModelTable {
            schema: Some("dbo".into()),
            name: "Roles".into(),
            columns: vec![
                col("id", "int", false, true, true, None),
                col("since", "date", true, false, false, None),
            ],
        };
        let tables = [a, b];
        let db = DatabaseType::SqlServer;

        let ts = generate_models(ModelLanguage::TypeScript, db, &tables);
        assert!(ts.contains("export interface User {"), "{ts}");
        assert!(ts.contains("export interface Role {"), "{ts}");
        assert_eq!(ts.matches("Generated by SQLLY").count(), 1);

        let zod = generate_models(ModelLanguage::Zod, db, &tables);
        assert_eq!(zod.matches("import { z }").count(), 1, "{zod}");
        assert!(zod.contains("RoleSchema"), "{zod}");

        let go = generate_models(ModelLanguage::Go, db, &tables);
        assert_eq!(go.matches("package models").count(), 1, "{go}");
        assert!(
            go.contains("type User struct") && go.contains("type Role struct"),
            "{go}"
        );

        let py = generate_models(ModelLanguage::Python, db, &tables);
        assert_eq!(
            py.matches("from dataclasses import dataclass").count(),
            1,
            "{py}"
        );
        assert_eq!(py.matches("from datetime import date\n").count(), 1, "{py}");

        let java = generate_models(ModelLanguage::Java, db, &tables);
        assert!(!java.contains("public record"), "{java}");
        assert!(
            java.contains("record User(") && java.contains("record Role("),
            "{java}"
        );

        let sa = generate_models(ModelLanguage::SqlAlchemy, db, &tables);
        assert_eq!(sa.matches("class Base(DeclarativeBase)").count(), 1, "{sa}");
        assert!(sa.contains("class Role(Base):"), "{sa}");

        let prisma = generate_models(ModelLanguage::Prisma, db, &tables);
        assert!(
            prisma.contains("model User {") && prisma.contains("model Role {"),
            "{prisma}"
        );

        let cs = generate_models(ModelLanguage::CSharp, db, &tables);
        assert_eq!(cs.matches("using System;").count(), 1, "{cs}");
        assert!(cs.contains("public record Role"), "{cs}");

        let rs = generate_models(ModelLanguage::Rust, db, &tables);
        assert_eq!(rs.matches("use serde::").count(), 1, "{rs}");
        assert!(rs.contains("pub struct Role {"), "{rs}");

        let kt = generate_models(ModelLanguage::Kotlin, db, &tables);
        assert_eq!(
            kt.matches("import java.time.LocalDate\n").count(),
            1,
            "{kt}"
        );
        assert!(kt.contains("data class Role("), "{kt}");
    }

    #[test]
    fn same_named_tables_in_two_schemas_get_distinct_type_names() {
        let a = ModelTable {
            schema: Some("sales".into()),
            name: "orders".into(),
            columns: vec![col("id", "int", false, true, false, None)],
        };
        let b = ModelTable {
            schema: Some("archive".into()),
            name: "orders".into(),
            columns: vec![col("id", "int", false, true, false, None)],
        };
        let out = generate_models(ModelLanguage::TypeScript, DatabaseType::Postgres, &[a, b]);
        assert!(out.contains("interface Order {"), "{out}");
        assert!(out.contains("interface Order_2 {"), "{out}");
    }

    #[test]
    fn column_names_follow_each_languages_casing_with_reserved_words_escaped() {
        let table = ModelTable {
            schema: None,
            name: "things".into(),
            columns: vec![
                col("First Name", "text", false, false, false, None),
                col("class", "int", false, false, false, None),
                col("2nd", "int", false, false, false, None),
            ],
        };
        let db = DatabaseType::Sqlite;
        let ts = generate_model(ModelLanguage::TypeScript, db, &table);
        assert!(
            ts.contains("  firstName: string; // First Name: text"),
            "{ts}"
        );
        assert!(ts.contains("  class_: number; // class: int"), "{ts}");
        assert!(ts.contains("  c2nd: number; // 2nd: int"), "{ts}");
        let py = generate_model(ModelLanguage::Python, db, &table);
        assert!(
            py.contains("    first_name: str  # First Name: text"),
            "{py}"
        );
        assert!(py.contains("    class_: int  # class: int"), "{py}");
        let cs = generate_model(ModelLanguage::CSharp, db, &table);
        assert!(cs.contains("public string FirstName"), "{cs}");
        assert!(cs.contains("[Column(\"First Name\")]"), "{cs}");
        assert!(cs.contains("public int Class_"), "{cs}");
        let rs = generate_model(ModelLanguage::Rust, db, &table);
        assert!(
            rs.contains("#[serde(rename = \"First Name\")]\n    pub first_name: String"),
            "{rs}"
        );
        let go = generate_model(ModelLanguage::Go, db, &table);
        assert!(
            go.contains("FirstName string `json:\"First Name\" db:\"First Name\"`"),
            "{go}"
        );
        assert!(go.contains("C2nd"), "{go}");
    }

    #[test]
    fn result_columns_make_a_model_without_keys() {
        let columns = vec![
            ResultColumn {
                name: "OrderID".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "Placed".into(),
                data_type: "datetime".into(),
                nullable: true,
            },
        ];
        let table = ModelTable::from_result_columns("Result", &columns);
        let out = generate_model(ModelLanguage::TypeScript, DatabaseType::SqlServer, &table);
        assert!(out.contains("export interface Result {"), "{out}");
        assert!(
            out.contains("  orderId: number; // OrderID: int\n"),
            "{out}"
        );
        assert!(
            out.contains("  placed: Date | null; // Placed: datetime\n"),
            "{out}"
        );
        assert!(!out.contains("primary key"), "{out}");
    }

    #[test]
    fn shape_carries_primary_key_identity_and_defaults() {
        let shape = sqlly_services::TableShape {
            schema: Some("dbo".into()),
            table: "T".into(),
            columns: vec![
                sqlly_services::ColumnDef {
                    name: "a".into(),
                    data_type: "int".into(),
                    nullable: false,
                    identity: true,
                    default: None,
                    default_constraint_name: None,
                },
                sqlly_services::ColumnDef {
                    name: "b".into(),
                    data_type: "int".into(),
                    nullable: true,
                    identity: false,
                    default: Some("((7))".into()),
                    default_constraint_name: Some("DF_T_b".into()),
                },
            ],
            primary_key: vec!["a".into()],
            pk_constraint_name: None,
        };
        let table = ModelTable::from_shape(&shape);
        assert!(table.columns[0].primary_key && table.columns[0].identity);
        assert!(!table.columns[1].primary_key);
        assert_eq!(table.columns[1].default.as_deref(), Some("((7))"));
        let out = generate_model(ModelLanguage::Prisma, DatabaseType::SqlServer, &table);
        assert!(
            out.contains("a Int  @id @default(autoincrement()) @db.Int"),
            "{out}"
        );
        assert!(out.contains("b Int? @default(7) @db.Int"), "{out}");
    }
}
