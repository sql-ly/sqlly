//! The built-in vocabulary of each SQL dialect: functions (with every
//! overload, per-parameter documentation, and the fixed words some
//! parameters take), keywords, constants, and data types.
//!
//! One catalog feeds every surface that needs to know what a word *is*:
//! syntax highlighting (which color), hover cards (what it means), signature
//! help (what each argument expects), and completion (which functions exist
//! and which words an argument slot accepts).
//!
//! The data lives in `data/builtins/*.json` beside this crate and is embedded
//! at compile time. `common.json` holds what every supported dialect shares;
//! each dialect file adds its own entries and may replace a common entry by
//! repeating its name (the dialect's entry wins whole). Files are parsed on
//! first use per dialect, never on the keystroke path after that.
//!
//! File format (all arrays optional):
//!
//! ```json
//! {
//!   "functions": [{
//!     "name": "DATEADD", "category": "Date and time",
//!     "summary": "Adds an interval to a date and returns the new date.",
//!     "example": "DATEADD(day, 7, GETDATE())",
//!     "niladic": false,
//!     "signatures": [{
//!       "returns": "same type as date",
//!       "variadic": false,
//!       "params": [{ "name": "datepart", "type": "keyword",
//!                    "doc": "The unit to add.", "optional": false,
//!                    "values": ["year", "month", "day"] }]
//!     }]
//!   }],
//!   "keywords":  [{ "name": "GROUP BY", "doc": "…", "syntax": "GROUP BY expr [, …]" }],
//!   "constants": [{ "name": "NULL", "doc": "…" }],
//!   "types":     [{ "name": "varchar", "doc": "…", "syntax": "varchar(n | max)" }]
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;
use sqlly_lang::sqlmodel::Dialect;

/// One parameter of a built-in function signature.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionParam {
    pub name: String,
    /// Expected type or kind of argument, in the dialect's own words
    /// (`int`, `string`, `date`, `keyword`, `any`).
    #[serde(rename = "type", default)]
    pub ty: String,
    pub doc: String,
    /// The argument may be left out (shown in brackets).
    #[serde(default)]
    pub optional: bool,
    /// The fixed words this slot accepts (`day`, `month`, … for a date part).
    /// Highlighted as constants in the slot and offered as completions.
    #[serde(default)]
    pub values: Vec<String>,
}

/// One overload of a built-in function.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionSignature {
    #[serde(default)]
    pub params: Vec<FunctionParam>,
    pub returns: String,
    /// The last parameter repeats (`COALESCE(value, value, …)`).
    #[serde(default)]
    pub variadic: bool,
}

/// A byte range inside a signature label.
pub type LabelRange = (usize, usize);

impl FunctionSignature {
    /// `NAME(a, b[, c])` or `NAME(a, …)`, with the byte range of each
    /// parameter's name inside the label (for bolding the active one).
    pub fn label(&self, name: &str) -> (String, Vec<LabelRange>) {
        let mut label = format!("{name}(");
        let mut ranges = Vec::with_capacity(self.params.len());
        for (i, param) in self.params.iter().enumerate() {
            let optional = param.optional;
            if optional {
                label.push('[');
            }
            if i > 0 {
                label.push_str(", ");
            }
            let start = label.len();
            label.push_str(&param.name);
            ranges.push((start, label.len()));
            if optional {
                label.push(']');
            }
        }
        if self.variadic && !self.params.is_empty() {
            label.push_str(", …");
        }
        label.push(')');
        (label, ranges)
    }

    /// The parameter an argument at zero-based `index` binds to: the
    /// matching one, or the repeating last one of a variadic signature.
    pub fn param_for_arg(&self, index: usize) -> Option<usize> {
        if index < self.params.len() {
            Some(index)
        } else if self.variadic && !self.params.is_empty() {
            Some(self.params.len() - 1)
        } else {
            None
        }
    }

    /// Whether a call with an argument at `index` fits this signature.
    pub fn accepts_arg(&self, index: usize) -> bool {
        self.param_for_arg(index).is_some()
    }
}

/// A built-in function of one dialect.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinFunction {
    pub name: String,
    pub category: String,
    pub summary: String,
    pub signatures: Vec<FunctionSignature>,
    #[serde(default)]
    pub example: Option<String>,
    /// Called without parentheses (`CURRENT_TIMESTAMP`).
    #[serde(default)]
    pub niladic: bool,
}

impl BuiltinFunction {
    /// Label for overload `index`: `NAME(a, b)`, or the bare name for a
    /// parameterless form of a function called without parentheses
    /// (`@@ROWCOUNT`, `CURRENT_TIMESTAMP`). Also returns the parameter byte
    /// ranges inside the label.
    pub fn label(&self, index: usize) -> (String, Vec<LabelRange>) {
        let Some(signature) = self.signatures.get(index) else {
            return (self.name.clone(), Vec::new());
        };
        if self.niladic && signature.params.is_empty() {
            return (self.name.clone(), Vec::new());
        }
        signature.label(&self.name)
    }

    /// The overload that best fits a call whose cursor sits in argument
    /// `active` (zero-based): the first that accepts that position, else the
    /// first overload.
    pub fn best_signature(&self, active: usize) -> usize {
        self.signatures
            .iter()
            .position(|s| s.accepts_arg(active))
            .unwrap_or(0)
    }
}

/// Documentation for a keyword (possibly a phrase such as `GROUP BY`), a
/// constant, or a data type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WordDoc {
    pub name: String,
    pub doc: String,
    #[serde(default)]
    pub syntax: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinFile {
    /// Free-form note for maintainers; ignored.
    #[serde(default, rename = "about")]
    _about: Option<String>,
    #[serde(default)]
    functions: Vec<BuiltinFunction>,
    #[serde(default)]
    keywords: Vec<WordDoc>,
    #[serde(default)]
    constants: Vec<WordDoc>,
    #[serde(default)]
    types: Vec<WordDoc>,
    /// Server system variables read as `@@name` (MySQL/MariaDB). SQL
    /// Server's `@@` names are documented as functions instead.
    #[serde(default)]
    variables: Vec<WordDoc>,
}

const COMMON_JSON: &str = include_str!("../../data/builtins/common.json");
const SQLSERVER_JSON: &str = include_str!("../../data/builtins/sqlserver.json");
const POSTGRES_JSON: &str = include_str!("../../data/builtins/postgres.json");
const MYSQL_JSON: &str = include_str!("../../data/builtins/mysql.json");
const SQLITE_JSON: &str = include_str!("../../data/builtins/sqlite.json");
const ORACLE_JSON: &str = include_str!("../../data/builtins/oracle.json");

fn dialect_json(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::SqlServer => SQLSERVER_JSON,
        Dialect::Postgres => POSTGRES_JSON,
        Dialect::MySql => MYSQL_JSON,
        Dialect::Sqlite => SQLITE_JSON,
        Dialect::Oracle => ORACLE_JSON,
    }
}

/// The merged vocabulary of one dialect. Lookups are case-insensitive.
#[derive(Debug)]
pub struct BuiltinCatalog {
    pub dialect: Dialect,
    functions: Vec<BuiltinFunction>,
    function_index: HashMap<String, usize>,
    keywords: HashMap<String, WordDoc>,
    constants: HashMap<String, WordDoc>,
    types: HashMap<String, WordDoc>,
    variables: Vec<WordDoc>,
    keyword_words: HashSet<String>,
    function_words: HashSet<String>,
    constant_words: HashSet<String>,
    type_words: HashSet<String>,
    longest_keyword_phrase: usize,
}

fn fold(word: &str) -> String {
    word.to_ascii_uppercase()
}

fn parse_file(name: &str, json: &str) -> BuiltinFile {
    match serde_json::from_str(json) {
        Ok(file) => file,
        Err(error) => {
            // The files are validated by tests; a broken one in a build must
            // cost highlighting quality, never the app.
            tracing::error!("builtin vocabulary {name} failed to parse: {error}");
            BuiltinFile::default()
        }
    }
}

impl BuiltinCatalog {
    fn build(dialect: Dialect) -> Self {
        let mut catalog = BuiltinCatalog {
            dialect,
            functions: Vec::new(),
            function_index: HashMap::new(),
            keywords: HashMap::new(),
            constants: HashMap::new(),
            types: HashMap::new(),
            variables: Vec::new(),
            keyword_words: HashSet::new(),
            function_words: HashSet::new(),
            constant_words: HashSet::new(),
            type_words: HashSet::new(),
            longest_keyword_phrase: 1,
        };
        catalog.merge(parse_file("common.json", COMMON_JSON));
        catalog.merge(parse_file("dialect", dialect_json(dialect)));
        catalog.index_words();
        catalog
    }

    fn merge(&mut self, file: BuiltinFile) {
        for function in file.functions {
            let key = fold(&function.name);
            match self.function_index.get(&key) {
                Some(&i) => self.functions[i] = function,
                None => {
                    self.function_index.insert(key, self.functions.len());
                    self.functions.push(function);
                }
            }
        }
        for keyword in file.keywords {
            self.keywords.insert(fold(&keyword.name), keyword);
        }
        for constant in file.constants {
            self.constants.insert(fold(&constant.name), constant);
        }
        for ty in file.types {
            self.types.insert(fold(&ty.name), ty);
        }
        for variable in file.variables {
            match self
                .variables
                .iter()
                .position(|v| v.name.eq_ignore_ascii_case(&variable.name))
            {
                Some(i) => self.variables[i] = variable,
                None => self.variables.push(variable),
            }
        }
    }

    fn index_words(&mut self) {
        for phrase in self.keywords.keys() {
            let words: Vec<&str> = phrase.split_whitespace().collect();
            self.longest_keyword_phrase = self.longest_keyword_phrase.max(words.len());
            for word in words {
                self.keyword_words.insert(word.to_ascii_lowercase());
            }
        }
        self.function_words = self
            .functions
            .iter()
            .map(|f| f.name.to_ascii_lowercase())
            .collect();
        self.constant_words = self
            .constants
            .keys()
            .map(|k| k.to_ascii_lowercase())
            .collect();
        self.type_words = self.types.keys().map(|k| k.to_ascii_lowercase()).collect();
    }

    /// Every built-in function, in file order (common first).
    pub fn functions(&self) -> &[BuiltinFunction] {
        &self.functions
    }

    pub fn function(&self, name: &str) -> Option<&BuiltinFunction> {
        self.function_index
            .get(&fold(name))
            .map(|&i| &self.functions[i])
    }

    /// A keyword or keyword phrase (`GROUP BY`), words separated by single
    /// spaces.
    pub fn keyword(&self, phrase: &str) -> Option<&WordDoc> {
        let normalized = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
        self.keywords.get(&fold(&normalized))
    }

    pub fn constant(&self, name: &str) -> Option<&WordDoc> {
        self.constants.get(&fold(name))
    }

    pub fn data_type(&self, name: &str) -> Option<&WordDoc> {
        self.types.get(&fold(name))
    }

    /// A server system variable (`@@sql_mode`), matched case-insensitively.
    pub fn system_variable(&self, name: &str) -> Option<&WordDoc> {
        self.variables
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
    }

    /// Everything the dialect lets you read as `@@name`: its `@@` system
    /// functions (SQL Server) and its system variables (MySQL/MariaDB), as
    /// (name, one-line description) pairs in catalog order.
    pub fn at_at_names(&self) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = self
            .functions
            .iter()
            .filter(|f| f.name.starts_with("@@"))
            .map(|f| (f.name.as_str(), f.summary.as_str()))
            .collect();
        out.extend(
            self.variables
                .iter()
                .map(|v| (v.name.as_str(), v.doc.as_str())),
        );
        out
    }

    /// Most words in any keyword phrase (bounds the hover phrase search).
    pub fn longest_keyword_phrase(&self) -> usize {
        self.longest_keyword_phrase
    }

    pub fn keyword_docs(&self) -> impl Iterator<Item = &WordDoc> {
        self.keywords.values()
    }

    pub fn constant_docs(&self) -> impl Iterator<Item = &WordDoc> {
        self.constants.values()
    }

    pub fn type_docs(&self) -> impl Iterator<Item = &WordDoc> {
        self.types.values()
    }

    /// Word-level membership for the highlighter. `lower` must already be
    /// ASCII-lowercased.
    pub fn is_keyword_word(&self, lower: &str) -> bool {
        self.keyword_words.contains(lower)
    }
    pub fn is_function_word(&self, lower: &str) -> bool {
        self.function_words.contains(lower)
    }
    pub fn is_constant_word(&self, lower: &str) -> bool {
        self.constant_words.contains(lower)
    }
    pub fn is_type_word(&self, lower: &str) -> bool {
        self.type_words.contains(lower)
    }

    /// The fixed words argument `arg` of `function` accepts, across every
    /// overload that has such a slot.
    pub fn slot_values(&self, function: &str, arg: usize) -> Vec<&str> {
        let Some(f) = self.function(function) else {
            return Vec::new();
        };
        let mut values = Vec::new();
        for signature in &f.signatures {
            if let Some(p) = signature.param_for_arg(arg) {
                for v in &signature.params[p].values {
                    if !values.iter().any(|x: &&str| x.eq_ignore_ascii_case(v)) {
                        values.push(v.as_str());
                    }
                }
            }
        }
        values
    }

    /// The parameter documentation for a fixed word in a slot, when `word`
    /// is one of the values argument `arg` of `function` accepts.
    pub fn slot_param(&self, function: &str, arg: usize, word: &str) -> Option<&FunctionParam> {
        let f = self.function(function)?;
        f.signatures.iter().find_map(|s| {
            let p = &s.params[s.param_for_arg(arg)?];
            p.values
                .iter()
                .any(|v| v.eq_ignore_ascii_case(word))
                .then_some(p)
        })
    }
}

/// The vocabulary for `dialect`, parsed once per process.
pub fn builtins(dialect: Dialect) -> &'static BuiltinCatalog {
    static CATALOGS: [OnceLock<BuiltinCatalog>; 5] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let slot = match dialect {
        Dialect::SqlServer => 0,
        Dialect::Postgres => 1,
        Dialect::MySql => 2,
        Dialect::Oracle => 3,
        Dialect::Sqlite => 4,
    };
    CATALOGS[slot].get_or_init(|| BuiltinCatalog::build(dialect))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: [(&str, &str); 6] = [
        ("common.json", COMMON_JSON),
        ("sqlserver.json", SQLSERVER_JSON),
        ("postgres.json", POSTGRES_JSON),
        ("mysql.json", MYSQL_JSON),
        ("sqlite.json", SQLITE_JSON),
        ("oracle.json", ORACLE_JSON),
    ];

    fn strict(name: &str, json: &str) -> BuiltinFile {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("{name} does not parse: {e}"))
    }

    fn is_word_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '@'
    }

    /// Structural rules every data file must follow. Collects every problem
    /// so one run reports a file's whole list.
    #[test]
    fn every_data_file_is_well_formed() {
        let mut problems = Vec::new();
        for (name, json) in FILES {
            let file = strict(name, json);
            let mut seen = HashSet::new();
            for f in &file.functions {
                let who = format!("{name}: function {}", f.name);
                if !seen.insert(format!("f:{}", fold(&f.name))) {
                    problems.push(format!("{who} is listed twice"));
                }
                if f.name.is_empty() || !f.name.chars().all(is_word_char) {
                    problems.push(format!("{who}: name must be one identifier"));
                }
                if f.category.trim().is_empty() {
                    problems.push(format!("{who}: empty category"));
                }
                if f.summary.trim().is_empty() || f.summary.len() > 320 {
                    problems.push(format!("{who}: summary must be 1–320 bytes"));
                }
                if f.signatures.is_empty() {
                    problems.push(format!("{who}: no signatures"));
                }
                if f.niladic && f.signatures.iter().all(|s| !s.params.is_empty()) {
                    problems.push(format!("{who}: niladic needs a parameterless signature"));
                }
                for (i, s) in f.signatures.iter().enumerate() {
                    if s.returns.trim().is_empty() {
                        problems.push(format!("{who}: signature {i} has no return type"));
                    }
                    if s.variadic && s.params.is_empty() {
                        problems.push(format!("{who}: signature {i} is variadic with no params"));
                    }
                    let mut optional_seen = false;
                    for p in &s.params {
                        if p.name.trim().is_empty() || p.doc.trim().is_empty() {
                            problems
                                .push(format!("{who}: signature {i} has an undocumented param"));
                        }
                        if p.doc.len() > 320 {
                            problems.push(format!("{who}: param {} doc over 320 bytes", p.name));
                        }
                        if optional_seen && !p.optional {
                            problems.push(format!(
                                "{who}: required param {} follows an optional one",
                                p.name
                            ));
                        }
                        optional_seen |= p.optional;
                    }
                }
            }
            for v in &file.variables {
                if !v.name.starts_with("@@") || v.name.len() < 3 {
                    problems.push(format!(
                        "{name}: variable {} must be spelled @@name",
                        v.name
                    ));
                }
            }
            for (label, docs) in [
                ("keyword", &file.keywords),
                ("constant", &file.constants),
                ("type", &file.types),
                ("variable", &file.variables),
            ] {
                for d in docs.iter() {
                    let who = format!("{name}: {label} {}", d.name);
                    if !seen.insert(format!("{label}:{}", fold(&d.name))) {
                        problems.push(format!("{who} is listed twice"));
                    }
                    if d.doc.trim().is_empty() || d.doc.len() > 400 {
                        problems.push(format!("{who}: doc must be 1–400 bytes"));
                    }
                    // `@@session.sql_mode`: variables may carry a scope.
                    let word_char = |c: char| is_word_char(c) || (label == "variable" && c == '.');
                    let words_ok = d
                        .name
                        .split(' ')
                        .all(|w| !w.is_empty() && w.chars().all(word_char));
                    let phrase_ok = label == "keyword" || !d.name.contains(' ');
                    if !words_ok || !phrase_ok {
                        problems.push(format!("{who}: bad name (single spaces between words)"));
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} problem(s):\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    /// A word painted as a constant must not also be listed as a keyword —
    /// the two get different colors, and the constant would lose.
    #[test]
    fn constants_are_never_keywords() {
        for dialect in Dialect::ALL {
            let c = builtins(dialect);
            for k in c.constants.keys() {
                assert!(
                    !c.is_keyword_word(&k.to_ascii_lowercase()),
                    "{dialect:?}: {k} is both a constant and a keyword word"
                );
            }
        }
    }

    #[test]
    fn dialect_entries_replace_common_ones() {
        let file = BuiltinFile {
            functions: vec![],
            ..Default::default()
        };
        let mut c = BuiltinCatalog {
            dialect: Dialect::SqlServer,
            functions: vec![],
            function_index: HashMap::new(),
            keywords: HashMap::new(),
            constants: HashMap::new(),
            types: HashMap::new(),
            variables: Vec::new(),
            keyword_words: HashSet::new(),
            function_words: HashSet::new(),
            constant_words: HashSet::new(),
            type_words: HashSet::new(),
            longest_keyword_phrase: 1,
        };
        c.merge(file);
        let make = |summary: &str| BuiltinFunction {
            name: "Trim".into(),
            category: "String".into(),
            summary: summary.into(),
            signatures: vec![FunctionSignature {
                params: vec![],
                returns: "string".into(),
                variadic: false,
            }],
            example: None,
            niladic: false,
        };
        c.merge(BuiltinFile {
            functions: vec![make("common")],
            keywords: vec![WordDoc {
                name: "group by".into(),
                doc: "Groups.".into(),
                syntax: None,
            }],
            ..Default::default()
        });
        c.merge(BuiltinFile {
            functions: vec![make("dialect")],
            ..Default::default()
        });
        c.index_words();
        assert_eq!(c.functions().len(), 1);
        assert_eq!(c.function("TRIM").unwrap().summary, "dialect");
        assert_eq!(c.keyword("Group   By").unwrap().doc, "Groups.");
        assert!(c.is_keyword_word("group") && c.is_keyword_word("by"));
        assert_eq!(c.longest_keyword_phrase(), 2);
    }

    #[test]
    fn labels_mark_optional_and_repeating_params() {
        let p = |name: &str, optional: bool| FunctionParam {
            name: name.into(),
            ty: String::new(),
            doc: "d".into(),
            optional,
            values: vec![],
        };
        let s = FunctionSignature {
            params: vec![
                p("data_type", false),
                p("expression", false),
                p("style", true),
            ],
            returns: "data_type".into(),
            variadic: false,
        };
        let (label, ranges) = s.label("CONVERT");
        assert_eq!(label, "CONVERT(data_type, expression[, style])");
        assert_eq!(&label[ranges[2].0..ranges[2].1], "style");
        let v = FunctionSignature {
            params: vec![p("value", false)],
            returns: "any".into(),
            variadic: true,
        };
        assert_eq!(v.label("COALESCE").0, "COALESCE(value, …)");
        assert_eq!(v.param_for_arg(5), Some(0));
        assert_eq!(s.param_for_arg(3), None);
    }
}

#[cfg(test)]
mod coverage {
    //! Guards against a data regeneration silently dropping vocabulary the
    //! editor relies on: each dialect must keep the words users type most.
    use super::*;

    fn assert_all(
        dialect: Dialect,
        what: &str,
        words: &[&str],
        has: impl Fn(&BuiltinCatalog, &str) -> bool,
    ) {
        let c = builtins(dialect);
        let missing: Vec<_> = words.iter().filter(|w| !has(c, w)).collect();
        assert!(
            missing.is_empty(),
            "{dialect:?} is missing {what}: {missing:?}"
        );
    }

    #[test]
    fn every_dialect_keeps_its_core_functions() {
        let f = |c: &BuiltinCatalog, w: &str| c.function(w).is_some();
        let shared = [
            "COUNT",
            "SUM",
            "AVG",
            "MIN",
            "MAX",
            "COALESCE",
            "NULLIF",
            "CAST",
            "UPPER",
            "LOWER",
            "ROUND",
            "REPLACE",
            "TRIM",
            "ROW_NUMBER",
            "RANK",
            "LAG",
            "LEAD",
        ];
        for dialect in Dialect::ALL {
            assert_all(dialect, "functions", &shared, f);
        }
        assert_all(
            Dialect::SqlServer,
            "functions",
            &[
                "GETDATE",
                "DATEADD",
                "DATEDIFF",
                "DATEPART",
                "ISNULL",
                "IIF",
                "LEN",
                "CHARINDEX",
                "SUBSTRING",
                "CONVERT",
                "TRY_CAST",
                "STRING_AGG",
                "FORMAT",
                "NEWID",
                "SCOPE_IDENTITY",
                "OBJECT_ID",
                "JSON_VALUE",
                "@@ROWCOUNT",
                "EOMONTH",
                "CONCAT",
            ],
            f,
        );
        assert_all(
            Dialect::Postgres,
            "functions",
            &[
                "now",
                "date_trunc",
                "extract",
                "to_char",
                "string_agg",
                "array_agg",
                "jsonb_build_object",
                "generate_series",
                "coalesce",
                "length",
                "substring",
                "regexp_replace",
                "gen_random_uuid",
                "age",
                "unnest",
                "split_part",
            ],
            f,
        );
        assert_all(
            Dialect::MySql,
            "functions",
            &[
                "NOW",
                "IFNULL",
                "IF",
                "DATE_ADD",
                "DATE_FORMAT",
                "TIMESTAMPDIFF",
                "GROUP_CONCAT",
                "JSON_EXTRACT",
                "CONCAT",
                "SUBSTRING_INDEX",
                "LAST_INSERT_ID",
                "UUID",
                "STR_TO_DATE",
            ],
            f,
        );
        assert_all(
            Dialect::Sqlite,
            "functions",
            &[
                "datetime",
                "strftime",
                "date",
                "julianday",
                "ifnull",
                "iif",
                "instr",
                "substr",
                "json_extract",
                "group_concat",
                "printf",
                "typeof",
                "last_insert_rowid",
                "random",
            ],
            f,
        );
        assert_all(
            Dialect::Oracle,
            "functions",
            &[
                "NVL",
                "NVL2",
                "DECODE",
                "SYSDATE",
                "SYSTIMESTAMP",
                "TO_CHAR",
                "TO_DATE",
                "ADD_MONTHS",
                "LISTAGG",
                "SUBSTR",
                "INSTR",
                "TRUNC",
                "REGEXP_SUBSTR",
            ],
            f,
        );
    }

    #[test]
    fn every_dialect_keeps_its_core_keywords_and_types() {
        let k = |c: &BuiltinCatalog, w: &str| c.is_keyword_word(&w.to_ascii_lowercase());
        let shared = [
            "SELECT", "FROM", "WHERE", "JOIN", "GROUP", "ORDER", "BY", "INSERT", "UPDATE",
            "DELETE", "CREATE", "ALTER", "DROP", "AND", "OR", "NOT", "CASE", "WHEN", "THEN",
            "ELSE", "END", "WITH", "AS", "ON", "IN", "IS", "LIKE", "BETWEEN", "DISTINCT", "UNION",
            "HAVING",
        ];
        for dialect in Dialect::ALL {
            assert_all(dialect, "keywords", &shared, k);
        }
        assert_all(
            Dialect::SqlServer,
            "keywords",
            &[
                "TOP",
                "DECLARE",
                "EXEC",
                "BEGIN",
                "TRY",
                "CATCH",
                "MERGE",
                "OUTPUT",
                "GO",
                "PROCEDURE",
                "RETURN",
                "WHILE",
                "PRINT",
                "THROW",
                "APPLY",
                "NOLOCK",
                "PIVOT",
                "TRUNCATE",
                "IDENTITY",
            ],
            k,
        );
        assert_all(
            Dialect::Postgres,
            "keywords",
            &[
                "LIMIT",
                "OFFSET",
                "RETURNING",
                "ILIKE",
                "LATERAL",
                "CONFLICT",
                "RECURSIVE",
            ],
            k,
        );
        assert_all(
            Dialect::MySql,
            "keywords",
            &[
                "LIMIT",
                "OFFSET",
                "AUTO_INCREMENT",
                "ENGINE",
                "DUPLICATE",
                "IGNORE",
                "SHOW",
                // MariaDB's `EXECUTE IMMEDIATE 'sql'`.
                "IMMEDIATE",
            ],
            k,
        );
        assert_all(
            Dialect::Sqlite,
            "keywords",
            &[
                "LIMIT",
                "PRAGMA",
                "AUTOINCREMENT",
                "ROWID",
                "VACUUM",
                "ATTACH",
            ],
            k,
        );
        assert_all(
            Dialect::Oracle,
            "keywords",
            &[
                "CONNECT", "PRIOR", "MINUS", "ROWNUM", "DUAL", "FETCH", "PACKAGE",
            ],
            k,
        );

        let t = |c: &BuiltinCatalog, w: &str| c.data_type(w).is_some();
        assert_all(
            Dialect::SqlServer,
            "types",
            &[
                "int",
                "bigint",
                "nvarchar",
                "varchar",
                "datetime2",
                "decimal",
                "bit",
                "uniqueidentifier",
                "varbinary",
                "money",
            ],
            t,
        );
        assert_all(
            Dialect::Postgres,
            "types",
            &[
                "integer",
                "bigint",
                "text",
                "varchar",
                "timestamptz",
                "jsonb",
                "uuid",
                "boolean",
                "numeric",
                "bytea",
            ],
            t,
        );
        assert_all(
            Dialect::MySql,
            "types",
            &[
                "INT", "BIGINT", "VARCHAR", "TEXT", "DATETIME", "DECIMAL", "JSON", "BLOB",
                "TINYINT",
            ],
            t,
        );
        assert_all(
            Dialect::Sqlite,
            "types",
            &["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC"],
            t,
        );
        assert_all(
            Dialect::Oracle,
            "types",
            &["NUMBER", "VARCHAR2", "DATE", "TIMESTAMP", "CLOB", "BLOB"],
            t,
        );
    }

    #[test]
    fn date_part_slots_accept_their_units() {
        let c = builtins(Dialect::SqlServer);
        for f in ["DATEADD", "DATEDIFF", "DATEPART", "DATENAME"] {
            let values = c.slot_values(f, 0);
            for unit in ["year", "month", "day", "minute", "mi", "dd"] {
                assert!(
                    values.iter().any(|v| v.eq_ignore_ascii_case(unit)),
                    "{f} lacks {unit}"
                );
            }
        }
        let m = builtins(Dialect::MySql).slot_values("TIMESTAMPDIFF", 0);
        assert!(m.iter().any(|v| v.eq_ignore_ascii_case("DAY")));
    }
}
