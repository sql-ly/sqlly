//! The `-- sqlly …` directive language, lexed in exactly one place.
//!
//! Every consumer of the directive grammar goes through [`classify`] /
//! [`classify_body`]: the real parsers (`query_format`, `result_set_names`,
//! `result_pivot`, `result_layout`), the editor hover ([`directive_span_at`]),
//! editor diagnostics ([`directive_diagnostics`]), and the reference window
//! (the [`DIRECTIVES`] table, whose entries are keyed to [`DirectiveKind`]).
//! A comment is therefore a directive everywhere or nowhere — the surfaces
//! cannot drift apart.
//!
//! The grammar, in one place:
//! - `format` keeps its canonical strict spelling `-- sqlly format:` — one
//!   space each side of `sqlly`, tight colon. (`--sqlly format:` is the
//!   documented spelling trap; [`directive_diagnostics`] calls it out.)
//! - Every other family tolerates loose spacing between the dashes and the
//!   namespace: `--`, optional ASCII whitespace, `sqlly` (any case), then at
//!   least one ASCII whitespace character before the keyword:
//!   - `name <n>: <text>` — digits required, spaces allowed before the
//!     colon, `n >= 1`, non-empty `<text>`;
//!   - `view:` / `pivot:` / `result:` / `layout:` — colon tight after the
//!     keyword;
//!   - `lines [<n>]: <rows>` — optional 1-based result-set index before the
//!     colon (spaces allowed), digits only; no index means every result set;
//!   - `focus: <n>` — colon tight after the keyword; the 1-based result set
//!     shown by default;
//!   - `grid [<n>]: <settings>` — like `lines`, an optional 1-based result
//!     set index before the colon; the column arrangement of a result grid;
//!   - `conditional:` — colon tight after the keyword; one conditional
//!     formatting rule for a column;
//!   - `map [<n>]: <settings>` — indexed like `grid`; how a result is drawn
//!     on the map (see `map_view::directive`);
//!   - `<rows>x<columns>` — two plain unsigned integers joined by `x`/`X`
//!     (digits only: no sign, and a value that overflows `usize` is not a
//!     canvas).
//!
//! Whether a directive is *honored* stays with each parser (`format` reads
//! any line, the canvas only the first non-empty one, placements only lines
//! without code); this module answers only "is this comment a directive, and
//! which one". [`directive_diagnostics`] closes the gap between the two: a
//! directive written where its parser would silently ignore it — trailing SQL
//! code (every family except `name`), or `view:`/canvas off the first
//! non-empty line — squiggles with the reason.
//!
//! A `/* sqlly lock` block comment (written by the result's lock button) is
//! the one block comment whose `-- sqlly` lines are honored: every parser
//! reads the text through [`unwrap_lock_blocks`], which blanks the block's
//! opener and closer lines (same byte length, so offsets survive) and leaves
//! its directive lines as ordinary whole-line comments. Any other block
//! comment still switches the directives inside it off.
//!
//! Everything here is pure text scanning — no I/O, no UI, and no allocation
//! on the classification path (it runs on mouse move).

/// The directive families, for linking documentation to the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveKind {
    /// `-- sqlly format: column=<name>; <format>`
    Format,
    /// `-- sqlly name <n>: <text>`
    Name,
    /// `-- sqlly view:<grid|pivot>`
    View,
    /// `-- sqlly pivot: row:…; column:…; value:…`
    Pivot,
    /// `-- sqlly chart: kind:…; label:…; values:…`
    Chart,
    /// `-- sqlly result: <row>|<column>`
    Result,
    /// `-- sqlly <rows>x<columns>`
    Canvas,
    /// `-- sqlly param: @name <type> [= default]`
    Param,
    /// `-- sqlly set <name> = <value>`
    Set,
    /// `-- sqlly layout: <stacked|tabbed|canvas>`
    Layout,
    /// `-- sqlly lines [<n>]: <rows>`
    Lines,
    /// `-- sqlly focus: <n>`
    Focus,
    /// `--sqlly date:<phrase>` — the natural-language date a literal was
    /// written from, kept so the literal can be re-evaluated.
    Date,
    /// `-- sqlly grid [<n>]: order=…; hide=…; freeze=…; sort=…; …`
    Grid,
    /// `-- sqlly conditional: column=<name>; when=…; fill=…`
    Conditional,
    /// `-- sqlly column: <Name> = <formula>`
    Column,
    /// `-- sqlly map [<n>]: lat=…; lon=…; mode=…; …`
    Map,
}

/// A classified directive comment, borrowing its body from the input.
/// `Name` offsets are byte positions relative to the string given to
/// [`classify`] / [`classify_body`] (whichever was called).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive<'a> {
    /// Body is everything after the `-- sqlly format:` colon.
    Format { body: &'a str },
    /// `number` is the 1-based result-set index; `name` is the trimmed label,
    /// `name_start..name_end` its byte span in the input.
    Name {
        number: usize,
        name: &'a str,
        name_start: usize,
        name_end: usize,
    },
    /// Body is everything after the `view:` colon.
    View { body: &'a str },
    /// Body is everything after the `pivot:` colon.
    Pivot { body: &'a str },
    /// Body is everything after the `chart:` colon.
    Chart { body: &'a str },
    /// Body is everything after the `result:` colon.
    Result { body: &'a str },
    /// Body is everything after the `param:` colon; the strip's own parser
    /// (`crate::query_params`) owns the details.
    Param { body: &'a str },
    /// Body is everything after the `set` keyword and its required whitespace.
    Set { body: &'a str },
    /// Body is everything after the `layout:` colon.
    Layout { body: &'a str },
    /// `index` is the 1-based result set the allocation applies to (`None`
    /// for every result set); `body` is everything after the colon.
    Lines { index: Option<usize>, body: &'a str },
    /// Body is everything after the `focus:` colon.
    Focus { body: &'a str },
    /// Body is everything after the `date:` colon: the phrase as typed.
    Date { body: &'a str },
    /// `index` is the 1-based result set the settings apply to (`None` for
    /// every result set); `body` is everything after the colon.
    Grid { index: Option<usize>, body: &'a str },
    /// Body is everything after the `conditional:` colon.
    Conditional { body: &'a str },
    /// Body is everything after the `column:` colon.
    Column { body: &'a str },
    /// `index` is the 1-based result set the map settings apply to (`None`
    /// for every result set); `body` is everything after the colon.
    Map { index: Option<usize>, body: &'a str },
    /// The stacked-layout canvas dimensions, as written (zero is possible —
    /// the layout parser owns that diagnostic).
    Canvas { rows: usize, columns: usize },
}

impl Directive<'_> {
    // Consumed by the docs/grammar round-trip tests below.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn kind(&self) -> DirectiveKind {
        match self {
            Directive::Format { .. } => DirectiveKind::Format,
            Directive::Name { .. } => DirectiveKind::Name,
            Directive::View { .. } => DirectiveKind::View,
            Directive::Pivot { .. } => DirectiveKind::Pivot,
            Directive::Chart { .. } => DirectiveKind::Chart,
            Directive::Result { .. } => DirectiveKind::Result,
            Directive::Canvas { .. } => DirectiveKind::Canvas,
            Directive::Param { .. } => DirectiveKind::Param,
            Directive::Set { .. } => DirectiveKind::Set,
            Directive::Layout { .. } => DirectiveKind::Layout,
            Directive::Lines { .. } => DirectiveKind::Lines,
            Directive::Focus { .. } => DirectiveKind::Focus,
            Directive::Date { .. } => DirectiveKind::Date,
            Directive::Grid { .. } => DirectiveKind::Grid,
            Directive::Conditional { .. } => DirectiveKind::Conditional,
            Directive::Column { .. } => DirectiveKind::Column,
            Directive::Map { .. } => DirectiveKind::Map,
        }
    }
}

/// Classify a line comment given from its leading `--`.
pub fn classify(comment: &str) -> Option<Directive<'_>> {
    let body = comment.strip_prefix("--")?;
    classify_body(body).map(|directive| match directive {
        // Re-base the name span onto the caller's string (past the dashes).
        Directive::Name {
            number,
            name,
            name_start,
            name_end,
        } => Directive::Name {
            number,
            name,
            name_start: name_start + 2,
            name_end: name_end + 2,
        },
        other => other,
    })
}

/// Classify a line comment given as its body — the text after the `--`.
/// (Some scanners, like `result_layout`'s, hold only the body.)
pub fn classify_body(body: &str) -> Option<Directive<'_>> {
    // `format` first: its canonical spelling is stricter than the tolerant
    // namespace below, so it must not fall through to the keyword scan
    // (where `format` is deliberately not a keyword — the strict spelling is
    // the documented contract, and `directive_diagnostics` explains the trap).
    if let Some(rest) = strip_prefix_ci(body, " sqlly format:") {
        return Some(Directive::Format { body: rest });
    }

    let rest = skip_ascii_ws(body);
    let rest = strip_prefix_ci(rest, "sqlly")?;
    // At least one whitespace character after the namespace — `sqllything`
    // is not a directive.
    let rest = rest.strip_prefix(|c: char| c.is_ascii_whitespace())?;
    let rest = skip_ascii_ws(rest);

    if let Some(after) = strip_prefix_ci(rest, "view") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::View { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "pivot") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Pivot { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "chart") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Chart { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "result") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Result { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "param") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Param { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "layout") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Layout { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "focus") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Focus { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "date") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Date { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "column") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Column { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "conditional") {
        let body = after.strip_prefix(':')?;
        return Some(Directive::Conditional { body });
    }
    if let Some(after) = strip_prefix_ci(rest, "lines") {
        // `lines: <rows>` or `lines <n>: <rows>` — like `name <n>:`, with
        // the index optional.
        let (index, body) = indexed_body(after)?;
        return Some(Directive::Lines { index, body });
    }
    if let Some(after) = strip_prefix_ci(rest, "grid") {
        // `grid: <settings>` or `grid <n>: <settings>`, indexed like `lines`.
        let (index, body) = indexed_body(after)?;
        return Some(Directive::Grid { index, body });
    }
    if let Some(after) = strip_prefix_ci(rest, "map") {
        // `map: <settings>` or `map <n>: <settings>`, indexed like `grid`.
        let (index, body) = indexed_body(after)?;
        return Some(Directive::Map { index, body });
    }
    if let Some(after) = strip_prefix_ci(rest, "set") {
        let body = after.strip_prefix(|c: char| c.is_ascii_whitespace())?;
        return Some(Directive::Set {
            body: skip_ascii_ws(body),
        });
    }
    if let Some(after) = strip_prefix_ci(rest, "name") {
        let after = after.strip_prefix(|c: char| c.is_ascii_whitespace())?;
        let after = skip_ascii_ws(after);
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let number: usize = after[..digits].parse().ok()?;
        if number < 1 {
            return None;
        }
        let after = skip_ascii_ws(&after[digits..]);
        let after = after.strip_prefix(':')?;
        let name = after.trim_matches(|c: char| c.is_ascii_whitespace());
        if name.is_empty() {
            return None;
        }
        // `name` is a subslice of `body`, so pointer distance is its offset.
        let name_start = name.as_ptr() as usize - body.as_ptr() as usize;
        return Some(Directive::Name {
            number,
            name,
            name_start,
            name_end: name_start + name.len(),
        });
    }

    parse_canvas_shape(rest)
}

/// The `[<n>]: <body>` tail of an optionally indexed keyword (`lines`,
/// `grid`): `None` unless a colon follows. An index needs whitespace between
/// the keyword and its digits (`lines2:` is not a directive) and is 1-based.
pub(crate) fn indexed_body(after: &str) -> Option<(Option<usize>, &str)> {
    let after_kw = skip_ascii_ws(after);
    let digits = after_kw.bytes().take_while(u8::is_ascii_digit).count();
    let (index, rest_after) = if digits > 0 {
        if after.len() == after_kw.len() {
            return None;
        }
        let number: usize = after_kw[..digits].parse().ok()?;
        if number < 1 {
            return None;
        }
        (Some(number), skip_ascii_ws(&after_kw[digits..]))
    } else {
        (None, after_kw)
    };
    let body = rest_after.strip_prefix(':')?;
    Some((index, body))
}

/// `<rows>x<columns>`: two digit-only integers joined by one `x`/`X`.
fn parse_canvas_shape(rest: &str) -> Option<Directive<'static>> {
    let mut parts = rest.split(['x', 'X']);
    let (Some(rows), Some(columns), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let number = |s: &str| {
        let s = s.trim_matches(|c: char| c.is_ascii_whitespace());
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<usize>().ok())
            .flatten()
    };
    Some(Directive::Canvas {
        rows: number(rows)?,
        columns: number(columns)?,
    })
}

/// Whether a comment names the `sqlly` namespace at all — i.e. the author
/// meant to write a directive, so a failed [`classify`] deserves a diagnostic
/// rather than silence. Deliberately looser than the grammar: any run of
/// dashes counts (`---sqlly view:` is an attempt the parsers all ignore).
pub fn names_namespace(comment: &str) -> bool {
    let body = skip_ascii_ws(comment.trim_start_matches('-'));
    let Some(rest) = strip_prefix_ci(body, "sqlly") else {
        return false;
    };
    // A word boundary after the namespace — "sqllything" is not an attempt.
    matches!(rest.as_bytes().first(), None | Some(b' ' | b'\t' | b':'))
}

/// Whether the diagnostic span starting at `start` sits on a `-- sqlly …`
/// comment — a valid directive or a failed attempt at one — so its hover
/// popup can link to the formatting-directives reference.
pub fn is_directive_comment_at(content: &str, start: usize) -> bool {
    let mut start = start.min(content.len());
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }
    let line_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = content[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(content.len());
    let line = &content[line_start..line_end];
    line_comment_start(line).is_some_and(|rel| names_namespace(&line[rel..]))
}

/// Case-insensitive `strip_prefix` for an ASCII `prefix`, without allocating.
/// `None` when `s` is shorter than the prefix or the prefix doesn't match.
/// The prefix must be ASCII: a multibyte prefix whose length lands mid-char
/// in `s` simply fails the match (via `get`), it does not panic.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn skip_ascii_ws(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

/// The opener the lock button writes; matched case-insensitively and with
/// any spacing after `/*` (see [`is_lock_opener`]).
pub const LOCK_OPENER: &str = "/* sqlly lock";

/// One `/* sqlly lock` … `*/` block: byte spans of its opener and closer
/// lines (terminators excluded). `start..end` covers the whole block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockBlock {
    pub start: usize,
    pub end: usize,
    pub opener: (usize, usize),
    pub closer: (usize, usize),
}

/// Whether `line` opens a lock block: `/*`, optional spaces, `sqlly`,
/// whitespace, `lock` as a whole word — and no `*/` closing it on the line.
fn is_lock_opener(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("/*") else {
        return false;
    };
    let Some(rest) = strip_prefix_ci(skip_ascii_ws(rest), "sqlly") else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(|c: char| c.is_ascii_whitespace()) else {
        return false;
    };
    let Some(rest) = strip_prefix_ci(skip_ascii_ws(rest), "lock") else {
        return false;
    };
    !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') && !line.contains("*/")
}

/// Every well-formed lock block in `sql`, in document order. A block is
/// well formed when its opener starts a line, a later line is exactly `*/`
/// (surrounding whitespace aside), and no line between them opens or closes
/// another block comment — so the database sees exactly one comment and the
/// directive lines inside are the whole block. Anything else is an ordinary
/// block comment, whose directives stay switched off.
pub fn lock_blocks(sql: &str) -> Vec<LockBlock> {
    let mut out = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut line_start = 0usize;
    for raw in sql.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let span = (line_start, line_start + line.len());
        match open {
            None if is_lock_opener(line) => open = Some(span),
            None => {}
            Some(opener) if line.trim() == "*/" => {
                out.push(LockBlock {
                    start: opener.0,
                    end: span.1,
                    opener,
                    closer: span,
                });
                open = None;
            }
            Some(_) if line.contains("/*") || line.contains("*/") => {
                // Not a block the lock wrote; a new opener may still follow.
                open = is_lock_opener(line).then_some(span);
            }
            Some(_) => {}
        }
        line_start += raw.len() + 1;
    }
    out
}

/// `sql` with every lock block's opener and closer lines blanked to spaces,
/// byte for byte, so the `-- sqlly` lines inside read as ordinary whole-line
/// directives (the first of them as the query's first non-empty line) while
/// every offset stays valid for the original text. Borrows when there is no
/// lock block.
pub fn unwrap_lock_blocks(sql: &str) -> std::borrow::Cow<'_, str> {
    let blocks = lock_blocks(sql);
    if blocks.is_empty() {
        return std::borrow::Cow::Borrowed(sql);
    }
    let mut bytes = sql.as_bytes().to_vec();
    for block in blocks {
        for (s, e) in [block.opener, block.closer] {
            bytes[s..e].fill(b' ');
        }
    }
    // Whole lines were replaced by ASCII spaces, so the text is still UTF-8.
    std::borrow::Cow::Owned(String::from_utf8(bytes).expect("blanked lines stay UTF-8"))
}

/// Byte range `(start, end)` of the `-- sqlly …` directive comment that
/// contains `offset`, or `None` when the offset is not inside such a comment.
/// `start` is the position of the leading `--`; `end` is the end of the line.
///
/// Recognition is [`classify`] — exactly what the real parsers accept — so
/// the hover never advertises a form that would be ignored at run time.
/// Comment detection skips `--` sequences inside string literals and
/// bracketed identifiers on the line, matching the SQL-aware parsers. The
/// scan stays O(line length) — this runs on mouse move.
pub fn directive_span_at(content: &str, offset: usize) -> Option<(usize, usize)> {
    // The offset is caller-supplied; land it on a char boundary before slicing
    // so a mid-UTF-8 position can never panic.
    let mut offset = offset.min(content.len());
    while offset > 0 && !content.is_char_boundary(offset) {
        offset -= 1;
    }
    let line_start = content[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = content[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(content.len());
    let line = &content[line_start..line_end];
    let comment_rel = line_comment_start(line)?;
    let comment_start = line_start + comment_rel;
    classify(&line[comment_rel..])?;
    // The offset has to fall within the comment itself, not the code before it.
    if offset < comment_start || offset > line_end {
        return None;
    }
    Some((comment_start, line_end))
}

/// Like [`directive_span_at`], but only when `offset` sits on the directive's
/// `-- sqlly` lead-in (the dashes through the end of the `sqlly` keyword).
/// The editor's info popup keys off this so resting the pointer or reading
/// along the settings that follow does not keep throwing the popup up.
pub fn directive_keyword_span_at(content: &str, offset: usize) -> Option<(usize, usize)> {
    let (start, end) = directive_span_at(content, offset)?;
    let body = &content[start + 2..end];
    let keyword_end = start + 2 + (body.len() - skip_ascii_ws(body).len()) + "sqlly".len();
    (offset <= keyword_end).then_some((start, end))
}

/// Position of the `--` that starts this line's comment, ignoring `--` inside
/// single-quoted string literals, `[bracketed]` identifiers, and `/* … */`
/// block comments opened on this line (they nest, per T-SQL — a commented-out
/// directive is not a directive). Everything after a real `--` is comment, so
/// at most one comment starts per line.
/// Line-local by design: a multi-line construct spanning into this line can
/// fool it, but that is the same perf boundary the rest of hover keeps.
pub(crate) fn line_comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut in_bracket = false;
    let mut block_depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // A doubled '' escape toggles twice — net state stays in-string.
            b'\'' if !in_bracket && block_depth == 0 => in_string = !in_string,
            b'[' if !in_string && !in_bracket && block_depth == 0 => in_bracket = true,
            b']' if in_bracket => in_bracket = false,
            b'/' if !in_string && !in_bracket && bytes.get(i + 1) == Some(&b'*') => {
                block_depth += 1;
                i += 2;
                continue;
            }
            b'*' if block_depth > 0 && bytes.get(i + 1) == Some(&b'/') => {
                block_depth -= 1;
                i += 2;
                continue;
            }
            b'-' if !in_string
                && !in_bracket
                && block_depth == 0
                && bytes.get(i + 1) == Some(&b'-') =>
            {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A directive problem worth surfacing as an editor diagnostic: the byte span
/// of the offending comment and a human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveDiagnostic {
    pub start: usize,
    pub end: usize,
    pub message: String,
}

/// Scan `sql` for `-- sqlly …` comments the real parsers would silently
/// ignore, so a typo'd directive squiggles instead of doing nothing.
///
/// The checks all share the hover's comment detection and grammar:
/// - a comment that names the `sqlly` namespace but classifies as no
///   directive (with a targeted message for the `--sqlly format:` spelling
///   trap);
/// - a directive written after SQL code on the same line — every family's
///   parser except `name` only honors comments that are the whole line
///   (`name`'s SQL-aware scanner deliberately honors trailing comments);
/// - a `format:` directive the real parser finds no rule in (wrong/missing
///   `column=` or format key);
/// - a `view:` or canvas directive on a line the parsers never read (they are
///   only honored on the first non-empty line);
/// - a `view:` value that is neither `grid` nor `pivot` (the parser silently
///   falls back to grid);
/// - canvas layout problems the result-layout parser finds in the SQL itself
///   (see `canvas_layout_diagnostics`).
pub fn directive_diagnostics(sql: &str) -> Vec<DirectiveDiagnostic> {
    // A lock block's directive lines read as whole-line comments, and its
    // first one as the query's first line (offsets are unchanged).
    let unwrapped = unwrap_lock_blocks(sql);
    let sql = unwrapped.as_ref();
    let mut out = Vec::new();
    let mut line_start = 0usize;
    let mut first_non_empty_seen = false;
    for line in sql.split('\n') {
        let is_first_non_empty = !first_non_empty_seen && !line.trim().is_empty();
        if is_first_non_empty {
            first_non_empty_seen = true;
        }
        if let Some(rel) = line_comment_start(line) {
            let comment = &line[rel..];
            let start = line_start + rel;
            let end = line_start + line.len();
            let standalone = line[..rel].trim().is_empty();
            match classify(comment) {
                None if names_namespace(comment) => {
                    let message = if strip_prefix_ci(comment, "--sqlly format").is_some() {
                        crate::localization::text("editor.sqlly_directive.format_spacing")
                    } else {
                        crate::localization::text("editor.sqlly_directive.unrecognized")
                    };
                    out.push(DirectiveDiagnostic {
                        start,
                        end,
                        message,
                    });
                }
                // `name` is the one family honored anywhere on a line — its
                // SQL-aware scanner reads trailing comments by design.
                Some(Directive::Name { .. }) => {}
                // A date marker trails the predicate it was written for.
                Some(Directive::Date { .. }) => {}
                Some(_) if !standalone => {
                    out.push(DirectiveDiagnostic {
                        start,
                        end,
                        message: crate::localization::text("editor.sqlly_directive.not_standalone"),
                    });
                }
                Some(Directive::Format { .. }) => {
                    // The family matched; check the body against the real parser.
                    if crate::query_format::parse(comment).is_empty() {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::text(
                                "editor.sqlly_directive.format_invalid",
                            ),
                        });
                    }
                }
                Some(Directive::View { .. } | Directive::Canvas { .. }) if !is_first_non_empty => {
                    out.push(DirectiveDiagnostic {
                        start,
                        end,
                        message: crate::localization::text("editor.sqlly_directive.not_first_line"),
                    });
                }
                Some(Directive::View { body }) => {
                    // In the honored position; the parser silently treats any
                    // value other than pivot as grid — surface the typo.
                    let value = body.trim();
                    if !value.eq_ignore_ascii_case("grid")
                        && !value.eq_ignore_ascii_case("pivot")
                        && !value.eq_ignore_ascii_case("chart")
                        && !value.eq_ignore_ascii_case("map")
                    {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::text(
                                "editor.sqlly_directive.unknown_view",
                            ),
                        });
                    }
                }
                Some(Directive::Chart { body }) => {
                    if let Err(err) = crate::result_chart::parse_chart_body(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.chart_invalid",
                                &[("error", &err.message)],
                            ),
                        });
                    }
                }
                Some(Directive::Param { body }) => {
                    if let Err(err) = crate::query_params::parse_spec(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.param_invalid",
                                &[("error", &err)],
                            ),
                        });
                    }
                }
                Some(Directive::Layout { body }) => {
                    if crate::result_layout::parse_requested_layout(body).is_none() {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::text(
                                "editor.sqlly_directive.unknown_layout",
                            ),
                        });
                    }
                }
                Some(Directive::Lines { body, .. }) => {
                    if crate::result_layout::parse_lines_value(body).is_none() {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.lines_invalid",
                                &[("maximum", &crate::result_layout::MAX_LINES.to_string())],
                            ),
                        });
                    }
                }
                Some(Directive::Focus { body }) => {
                    if crate::result_layout::parse_focus_value(body).is_none() {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::text(
                                "editor.sqlly_directive.focus_invalid",
                            ),
                        });
                    }
                }
                Some(Directive::Grid { body, .. }) => {
                    if let Err(err) = crate::grid_directive::parse_body(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.grid_invalid",
                                &[("error", &err)],
                            ),
                        });
                    }
                }
                Some(Directive::Column { body }) => {
                    if let Err(err) = crate::computed_columns::parse_body(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.computed_column_invalid",
                                &[("error", &err)],
                            ),
                        });
                    }
                }
                Some(Directive::Conditional { body }) => {
                    if let Err(err) = crate::conditional_directive::parse_body(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.conditional_invalid",
                                &[("error", &err)],
                            ),
                        });
                    }
                }
                Some(Directive::Map { body, .. }) => {
                    let problems = crate::map_view::directive::diagnostics(body);
                    if !problems.is_empty() {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "map_view.directive.invalid",
                                &[("error", &problems.join("; "))],
                            ),
                        });
                    }
                }
                Some(Directive::Set { body }) => {
                    if let Err(err) = crate::query_params::parse_set_spec(body) {
                        out.push(DirectiveDiagnostic {
                            start,
                            end,
                            message: crate::localization::format(
                                "editor.sqlly_directive.set_invalid",
                                &[("error", &err)],
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
        line_start += line.len() + 1;
    }
    out.extend(canvas_layout_diagnostics(sql));
    out
}

/// The canvas layout parser's static problems (a malformed, out-of-board or
/// overlapping `-- sqlly result:` placement, a placement with no statement
/// after it, a statement with no open cell, an unusable board size), each
/// pinned to the line the parser names: the directive comment on that line
/// when the line is only a comment, else the line's trimmed text.
fn canvas_layout_diagnostics(sql: &str) -> Vec<DirectiveDiagnostic> {
    let spec = crate::result_layout::parse(sql);
    if spec.diagnostics.is_empty() {
        return Vec::new();
    }
    let lines = line_byte_ranges(sql);
    spec.diagnostics
        .iter()
        .filter_map(|d| {
            let (line_start, line_end) = *lines.get(d.line?.checked_sub(1)?)?;
            let text = &sql[line_start..line_end];
            let rel = match line_comment_start(text) {
                Some(rel) if text[..rel].trim().is_empty() => rel,
                _ => text.len() - text.trim_start().len(),
            };
            let start = line_start + rel;
            let end = line_start + text.trim_end().len();
            (start < end).then(|| DirectiveDiagnostic {
                start,
                end,
                message: d.message.clone(),
            })
        })
        .collect()
}

/// Byte range of each line of `sql`, excluding its terminator. Lines break
/// on `\r\n`, `\r` and `\n` alike, matching the layout parser's line
/// normalization so its 1-based line numbers index this directly.
fn line_byte_ranges(sql: &str) -> Vec<(usize, usize)> {
    let bytes = sql.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                ranges.push((start, i));
                start = i + 1;
            }
            b'\r' => {
                ranges.push((start, i));
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    ranges.push((start, bytes.len()));
    ranges
}

/// One documented directive family, for the reference window and grammar
/// round-trip tests.
pub struct DirectiveDoc {
    /// The grammar families this entry documents (empty for option fragments
    /// that are not directives on their own). Read by the round-trip tests,
    /// which hold every sample to its documented family.
    #[cfg_attr(not(test), allow(dead_code))]
    pub kinds: &'static [DirectiveKind],
    /// Plain-language name for the family (e.g. "Column format"), shown in
    /// the reference window's family list and as the card title.
    pub title: &'static str,
    /// The directive keyword shown as the heading (e.g. `-- sqlly format:`).
    pub heading: &'static str,
    /// One-line overview of what it controls.
    pub overview: &'static str,
    /// The syntax skeleton.
    pub syntax: &'static str,
    /// Parameters / allowed values, each an `(name, description)` pair.
    pub params: &'static [(&'static str, &'static str)],
    /// Worked examples, copy-pasteable.
    pub samples: &'static [&'static str],
}

/// Every directive SQLLY understands, in the order they're most commonly
/// used. Lives beside the grammar so docs and lexer stay one artifact; the
/// tests below hold every sample to [`classify`] and to the real parsers.
pub static DIRECTIVES: &[DirectiveDoc] = &[
    DirectiveDoc {
        kinds: &[DirectiveKind::Format],
        title: "Column format",
        heading: "-- sqlly format:",
        overview: "Formats how a single result column is displayed.",
        syntax: "-- sqlly format: column=<name>; <format>[; thousands=<true|false>]",
        params: &[
            (
                "column=<name>",
                "The result column this rule applies to (required).",
            ),
            (
                "number=<decimals>",
                "Fixed-point number with the given decimal places.",
            ),
            (
                "thousands=<true|false>",
                "Thousands separators for a number format (default true).",
            ),
            (
                "date=<pattern>",
                "strftime pattern, e.g. %Y-%m-%d or %d %b %Y.",
            ),
            (
                "boolean=<true-text>/<false-text>",
                "Text to show for true / false, e.g. Yes/No.",
            ),
            (
                "binary=<hex|base64|uuid|ulid>",
                "How to render binary/varbinary values; uuid and ulid show 16-byte ids as \
                 UUID or ULID text (other lengths as hex).",
            ),
            (
                "text=<upper|lower|title|none>",
                "Change the casing of a text column.",
            ),
            (
                "width=<n>dp",
                "Column width in font-relative units: 1dp is a quarter of the grid font size, so \
                 a column keeps about the same number of visible characters at any font size. \
                 May be the only setting on the line.",
            ),
        ],
        samples: &[
            "-- sqlly format: column=Total; number=2",
            "-- sqlly format: column=Qty; number=0; thousands=false",
            "-- sqlly format: column=Created; date=%Y-%m-%d",
            "-- sqlly format: column=IsActive; boolean=Yes/No",
            "-- sqlly format: column=Hash; binary=hex",
            "-- sqlly format: column=Code; text=upper",
            "-- sqlly format: column=Notes; width=60dp",
            "-- sqlly format: column=Total; number=2; width=40dp",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Name],
        title: "Result names",
        heading: "-- sqlly name",
        overview: "Names a result set — handy when a batch returns several.",
        syntax: "-- sqlly name <n>: <text>",
        params: &[
            ("<n>", "The 1-based index of the result set to name."),
            ("<text>", "The label to show on that result set's tab."),
        ],
        samples: &["-- sqlly name 1: Customers", "-- sqlly name 2: Open Orders"],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::View],
        title: "Result view",
        heading: "-- sqlly view:",
        overview: "Chooses how a result set is presented. Only honored on the \
                   first non-empty line of the query.",
        syntax: "-- sqlly view: <grid|pivot|chart|map>",
        params: &[
            ("grid", "The standard row/column results grid (default)."),
            (
                "pivot",
                "Render as a pivot table (configured by -- sqlly pivot:).",
            ),
            (
                "chart",
                "Open the Chart tab (configured by -- sqlly chart:).",
            ),
            (
                "map",
                "Also open the result on a map when the query finishes (configured by -- sqlly map:).",
            ),
        ],
        samples: &[
            "-- sqlly view: grid",
            "-- sqlly view: pivot",
            "-- sqlly view: chart",
            "-- sqlly view: map",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Pivot],
        title: "Pivot table",
        heading: "-- sqlly pivot:",
        overview: "Configures the pivot table when the view is pivot.",
        syntax: "-- sqlly pivot: row:<cols>; column:<cols>; value:<col>:<agg>[; format:<col>:<opts>][; valueformat:<opts>]",
        params: &[
            (
                "row:<col[,col…]>",
                "Column(s) that form the pivot rows (required).",
            ),
            (
                "column:<col[,col…]>",
                "Column(s) whose values become pivot columns (required).",
            ),
            (
                "value:<col>:<agg>",
                "Value column and aggregate: count, sum, avg, first, max, or min (required).",
            ),
            (
                "format:<col>:<opts>",
                "Per-field number format (see options below).",
            ),
            ("valueformat:<opts>", "Number format for the value cells."),
            (
                "result:<n>",
                "The 1-based result set the pivot belongs to (default 1).",
            ),
        ],
        samples: &[
            "-- sqlly pivot: row:Region; column:Year; value:Sales:sum",
            "-- sqlly pivot: row:Rep; column:Quarter; value:Deals:count; valueformat:decimals=0",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Chart],
        title: "Chart",
        heading: "-- sqlly chart:",
        overview: "Configures the Chart tab: type, category label, series, and \
                   aggregation. Pair with -- sqlly view: chart to open it on run.",
        syntax: "-- sqlly chart: kind:<type>[; label:<col>][; values:<cols>][; aggregate:<agg>][; order:<order>][; top:<n>][; navigate:<true|false>]",
        params: &[
            (
                "kind:<type>",
                "bar, line, area, scatter, histogram, pie, or donut (required).",
            ),
            (
                "label:<col>",
                "Column that labels the categories (default: first text column).",
            ),
            (
                "values:<col[,col…]>",
                "Numeric column(s) to plot as series (default: auto-detect).",
            ),
            (
                "aggregate:<agg>",
                "Collapse rows per category: none, sum, average, minimum, maximum, or count.",
            ),
            (
                "order:<order>",
                "Category order: source, category (A–Z), or value (high–low).",
            ),
            ("top:<n>", "Keep at most n categories after ordering."),
            (
                "navigate:<true|false>",
                "Clicking a mark selects its source rows on the Grid tab (default true).",
            ),
            (
                "result:<n>",
                "The 1-based result set the chart belongs to (default 1).",
            ),
        ],
        samples: &[
            "-- sqlly chart: kind:bar; label:Region; values:Sales",
            "-- sqlly chart: kind:line; label:Month; values:Revenue,Cost; aggregate:sum",
            "-- sqlly chart: kind:pie; label:Category; values:Total; order:value; top:10",
            "-- sqlly chart: kind:bar; label:Region; values:Sales; result:2",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Conditional],
        title: "Conditional formatting",
        heading: "-- sqlly conditional:",
        overview: "Colors a column's cells by their values, for this query only. One rule per \
                   line; the first matching when= rule for a column wins, and these rules come \
                   before the ones saved in the Conditional Formatting dialog.",
        syntax: "-- sqlly conditional: column=<name>; <when=<test>; style… | scale=<from>..<to> | bar=<color>>",
        params: &[
            (
                "column=<name>",
                "The result column the rule applies to (required).",
            ),
            (
                "when=<test>",
                "null, not null, > n, < n, between a and b, is <text>, or contains <text> \
                 (text tests ignore case).",
            ),
            ("text=#RRGGBB", "Text color for matching cells."),
            ("fill=#RRGGBB", "Background for matching cells."),
            ("bold", "Bold text for matching cells."),
            (
                "scale=#RRGGBB..#RRGGBB",
                "Color scale from the column minimum to its maximum (numbers).",
            ),
            (
                "bar=#RRGGBB",
                "A data bar proportional to each value (numbers).",
            ),
        ],
        samples: &[
            "-- sqlly conditional: column=Status; when=is Failed; text=#B00020; bold",
            "-- sqlly conditional: column=Balance; when=< 0; fill=#FDE7E9",
            "-- sqlly conditional: column=Latency; scale=#63BE7B..#F8696B",
            "-- sqlly conditional: column=Revenue; bar=#5B9BD5",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Column],
        title: "Computed column",
        heading: "-- sqlly column:",
        overview: "Adds a column computed with an Excel formula from the result's own columns. \
                   Columns are written !Name, the row before !Name[1], a fixed row !Name[$1], and \
                   declared variables @name; IF, COALESCE, text, math, and date functions work as \
                   in Excel, and an Excel export keeps the formula.",
        syntax: "-- sqlly column: <Name> = <formula>[; after=<column>][; result=<n>]",
        params: &[
            ("<Name>", "The new column's name."),
            ("<formula>", "An Excel formula; the leading = is optional."),
            (
                "!Name / !Name[1] / !Name[$1]",
                "This row, the row before, the first row.",
            ),
            ("!A[2]:!A", "A range: this row and the two before it."),
            ("@name", "A -- sqlly set or -- sqlly param: value."),
            (
                "after=<column>",
                "Place it after this column (default: at the end).",
            ),
            (
                "result=<n>",
                "Only the n-th result set (default: every one it fits).",
            ),
        ],
        samples: &[
            "-- sqlly column: Total = !Qty * !Price",
            "-- sqlly column: Margin % = IF(!Revenue = 0, 0, (!Revenue - !Cost) / !Revenue); after=Cost",
            "-- sqlly column: Running Total = !Amount + N(!\"Running Total\"[1])",
            "-- sqlly column: Due = TEXT(EDATE(!OrderDate, 1), \"yyyy-mm-dd\")",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Grid],
        title: "Grid columns",
        heading: "-- sqlly grid:",
        overview: "Arranges a result grid's columns: order, hidden and frozen columns, sort, \
                   grouping, column filters, and foreign-key labels. Wins over the layout SQLly \
                   remembers for the query.",
        syntax: "-- sqlly grid [<n>]: <setting>[; <setting>…]",
        params: &[
            (
                "<n>",
                "Optional 1-based result set; without it, every result set.",
            ),
            (
                "order=<cols>",
                "Display order; unlisted columns follow in their own order.",
            ),
            ("hide=<cols>", "Columns to hide."),
            ("freeze=<n>", "Pin the first n displayed columns."),
            (
                "sort=<col> [asc|desc], …",
                "Multi-column sort (asc by default).",
            ),
            ("group=<col>", "Group rows by a column."),
            (
                "filter=<col> <op> <value>",
                "A column filter (repeatable): contains, not contains, begins with, ends with, \
                 is, is not, matches, =, !=, >, >=, <, <=, between a and b, not between a and \
                 b, or in a | b | c.",
            ),
            (
                "label=<col>:<column|off>",
                "Foreign-key label shown beside a key column (repeatable).",
            ),
        ],
        samples: &[
            "-- sqlly grid: order=Id, Name, Total; hide=Notes; freeze=1",
            "-- sqlly grid: sort=Total desc, Name; filter=Status is Open",
            "-- sqlly grid 2: group=Region; filter=Total between 100 and 500",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Canvas, DirectiveKind::Result],
        title: "Canvas layout",
        heading: "-- sqlly <rows>x<columns> / -- sqlly result:",
        overview: "Lays several result sets out on one grid canvas. The canvas \
                   directive is only honored on the first non-empty line; each \
                   result: places the next result-producing statement.",
        syntax: "-- sqlly <rows>x<columns>   then, per statement:   -- sqlly result: <row>[-<row>]|<column>[-<column>]",
        params: &[
            (
                "<rows>x<columns>",
                "The canvas dimensions, e.g. 2x2 (first non-empty line only).",
            ),
            (
                "result: <row>|<column>",
                "1-based cell for the next result set; ranges span cells.",
            ),
        ],
        samples: &[
            "-- sqlly 2x2",
            "-- sqlly result: 1|1",
            "-- sqlly result: 1-2|2",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Layout, DirectiveKind::Lines],
        title: "Result layout",
        heading: "-- sqlly layout: / -- sqlly lines:",
        overview: "Chooses the result layout the run opens in and, for stacked \
                   cards, how many rows each result set is allotted. Both are \
                   honored anywhere in the script when the comment is the whole \
                   line; the first layout: wins, and a later lines: for the same \
                   result set overrides an earlier one.",
        syntax: "-- sqlly layout: <stacked|tabbed|canvas>   and   -- sqlly lines [<n>]: <rows>",
        params: &[
            (
                "layout: stacked | tabbed | canvas",
                "The layout to show the results in (tabs also accepted for tabbed).",
            ),
            (
                "lines: <rows>",
                "Rows allotted to every stacked card that has no allocation of its own.",
            ),
            (
                "lines <n>: <rows>",
                "Rows allotted to the n-th result set's stacked card (1-based). Dragging a card's divider still overrides it.",
            ),
        ],
        samples: &[
            "-- sqlly layout: stacked",
            "-- sqlly lines: 8",
            "-- sqlly lines 2: 20",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Focus],
        title: "Default result",
        heading: "-- sqlly focus:",
        overview: "Chooses which result set is in view when results arrive: its \
                   tab is selected in the tabbed layout, and its card is scrolled \
                   into view in the stacked layout. Honored anywhere in the script \
                   when the comment is the whole line; a later focus: overrides an \
                   earlier one.",
        syntax: "-- sqlly focus: <n>",
        params: &[(
            "<n>",
            "The 1-based index of the result set to show by default.",
        )],
        samples: &["-- sqlly focus: 2"],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Param],
        title: "Parameters",
        heading: "-- sqlly param:",
        overview: "Declares a query parameter. A parameter strip appears above the results where you type each value; Run substitutes them as typed, escaped literals.",
        syntax: "-- sqlly param: @name <type> [= default]",
        params: &[
            (
                "@name",
                "The placeholder to replace wherever it appears in the SQL (outside strings and comments).",
            ),
            (
                "<type>",
                "One of text, int, decimal, bool, date, datetime — the value is validated before Run.",
            ),
            (
                "= default",
                "Optional value used when the strip's field is left empty; type NULL for SQL NULL.",
            ),
        ],
        samples: &[
            "-- sqlly param: @CustomerId int",
            "-- sqlly param: @From date = 2026-01-01",
            "-- sqlly param: @Name text = %smith%",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Set],
        title: "Variables",
        heading: "-- sqlly set",
        overview: "Defines a text variable for this script. Every `${name}` outside strings and comments is replaced with the value verbatim before parameters are substituted and before the query runs. Later set lines override earlier ones for the same name.",
        syntax: "-- sqlly set <name> = <value>",
        params: &[
            (
                "<name>",
                "A letter/underscore identifier without @, $, or :.",
            ),
            (
                "= <value>",
                "Text pasted verbatim into every `${name}` placeholder (not quoted).",
            ),
        ],
        samples: &["-- sqlly set schema = dbo", "-- sqlly set suffix = _2026"],
    },
    DirectiveDoc {
        kinds: &[],
        title: "Pivot number options",
        heading: "format / valueformat options",
        overview: "Options shared by the pivot format: and valueformat: fields.",
        syntax: "<default | key=value[, key=value …]>",
        params: &[
            ("align=<left|center|right>", "Cell alignment."),
            ("decimals=<n>", "Number of decimal places."),
            ("red=<true|false>", "Show negative numbers in red."),
            (
                "parens=<true|false>",
                "Wrap negative numbers in parentheses.",
            ),
            ("thousands=<true|false>", "Thousands separators."),
        ],
        samples: &[
            "value:Amount:sum; valueformat:decimals=2, red=true",
            "format:Margin:decimals=1, parens=true, align=right",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Date],
        title: "Natural-language date",
        heading: "--sqlly date:",
        overview: "Written after a date you typed in plain words (Ctrl+Shift+Space after a \
                   date comparison). Keeps the words so the date can be re-evaluated with \
                   the refresh button at the end of the line.",
        syntax: "<column> <comparison> --sqlly date:<phrase>",
        params: &[(
            "<phrase>",
            "What you typed: today, last wed at 3, november, ytd, past 7 days…",
        )],
        samples: &[
            "WHERE OrderDate BETWEEN '2026-10-01' AND '2026-10-31' --sqlly date:this month",
            "WHERE created_at >= '2026-10-21T15:00:00' --sqlly date:yesterday at 3",
        ],
    },
    DirectiveDoc {
        kinds: &[DirectiveKind::Map],
        title: "Map",
        heading: "-- sqlly map:",
        overview: "Draws a result on the map: which columns hold the locations, labels, \
                   colors and sizes, the heatmap, clustering, the street map, the starting \
                   view, and what a click does. Every option is also in the map window's \
                   Options panel, whose Write to Query button writes this line.",
        syntax: "-- sqlly map [<n>]: key=value[; key=value …]",
        params: &[
            (
                "<n>",
                "The 1-based result set the settings apply to (default: every result).",
            ),
            (
                "lat=<col>; lon=<col>",
                "Latitude and longitude columns (override auto-detection).",
            ),
            (
                "geom=<col>",
                "A WKT or WKB geometry column (overrides auto-detection).",
            ),
            (
                "srid=<n>",
                "Coordinate system of the data; anything but 4326 draws without the street map.",
            ),
            ("label=<col>", "Column shown as the hover title."),
            (
                "tooltip=<col[, col…]>",
                "Extra columns listed in the hover tooltip.",
            ),
            (
                "mode=<points|heatmap|both>",
                "Draw markers, a heatmap, or both.",
            ),
            (
                "color=<col>",
                "Color markers by a column: a palette for text, a gradient for numbers.",
            ),
            ("size=<col>", "Scale marker size by a numeric column."),
            (
                "weight=<col>",
                "Weight each location's heat by a numeric column.",
            ),
            (
                "radius=<px>; blur=<px>",
                "Heatmap spread and extra softening, in screen pixels.",
            ),
            (
                "intensity=<0..10>; opacity=<0..1>",
                "Heatmap color boost and maximum opacity.",
            ),
            (
                "palette=<heat|fire|viridis|ocean|accent>",
                "Heatmap color ramp.",
            ),
            (
                "cluster=<on|off>",
                "Group nearby markers into counted circles.",
            ),
            (
                "basemap=<osm|none>",
                "Street map tiles, or none for this result (never bypasses your tile setting).",
            ),
            (
                "center=<lat>,<lon>; zoom=<0..19>",
                "Starting view; fit=data frames every feature instead.",
            ),
            ("limit=<n>", "Most features to draw (at most 50,000)."),
            (
                "click=<select|none>",
                "Whether clicking a feature selects its grid row.",
            ),
            (
                "open=<auto|manual>",
                "Open the map automatically when the query finishes.",
            ),
        ],
        samples: &[
            "-- sqlly map: lat=latitude; lon=longitude; label=store_name",
            "-- sqlly map: mode=heatmap; weight=orders; radius=30; palette=fire",
            "-- sqlly map 2: color=region; size=revenue; cluster=on; open=auto",
            "-- sqlly map: geom=boundary; srid=4326; tooltip=name, area; basemap=none",
            "-- sqlly map: center=51.5,-0.12; zoom=10; click=none",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_directive_family() {
        for line in [
            "-- sqlly format: column=Total; number=2",
            "-- sqlly name 1: Customers",
            "-- sqlly view: pivot",
            "-- sqlly pivot: row:Region; column:Year; value:Sales:sum",
            // The name/view/pivot parsers tolerate the no-space dash form.
            "--sqlly view: pivot",
            "--sqlly name 2: Orders",
            "-- SQLLY VIEW: grid",
        ] {
            let sql = format!("SELECT 1;\n{line}\nSELECT 2;");
            // A point a few characters into the comment (case-insensitive, so
            // locate it via the `--` rather than the keyword).
            let offset = sql.find("--").unwrap() + 3;
            assert!(
                directive_span_at(&sql, offset).is_some(),
                "should detect: {line}"
            );
        }
    }

    #[test]
    fn every_directive_diagnostic_sits_on_a_directive_comment() {
        for sql in [
            "SELECT 1\n-- sqlly format: colum=Total; number=2",
            "--sqlly format: column=X; number=0\nSELECT 1",
            "SELECT 1\n-- sqlly view: pivot",
            "SELECT 1 -- sqlly view: grid",
            "-- sqlly view: table\nSELECT 1",
        ] {
            let diags = directive_diagnostics(sql);
            assert!(!diags.is_empty(), "expected a diagnostic for {sql:?}");
            for d in diags {
                assert!(is_directive_comment_at(sql, d.start), "{sql:?}");
            }
        }
    }

    #[test]
    fn plain_comments_and_code_are_not_directive_comments() {
        let sql = "SELECT 1 -- just a note\nSELECT 'x -- sqlly view: pivot'";
        assert!(!is_directive_comment_at(sql, 0));
        assert!(!is_directive_comment_at(sql, sql.find("--").unwrap()));
        assert!(!is_directive_comment_at(sql, sql.len() - 5));
        assert!(!is_directive_comment_at(sql, usize::MAX));
    }

    #[test]
    fn classify_layout_and_lines() {
        assert_eq!(
            classify("-- sqlly layout: stacked"),
            Some(Directive::Layout { body: " stacked" })
        );
        assert_eq!(
            classify("-- sqlly lines: 10"),
            Some(Directive::Lines {
                index: None,
                body: " 10"
            })
        );
        assert_eq!(
            classify("-- sqlly lines 2 : 20"),
            Some(Directive::Lines {
                index: Some(2),
                body: " 20"
            })
        );
        assert!(classify("-- sqlly lines2: 20").is_none());
        assert!(classify("-- sqlly lines 0: 20").is_none());
        assert!(classify("-- sqlly layout stacked").is_none());
    }

    #[test]
    fn classify_focus() {
        assert_eq!(
            classify("-- sqlly focus: 2"),
            Some(Directive::Focus { body: " 2" })
        );
        assert_eq!(
            classify("--SQLLY   Focus:3"),
            Some(Directive::Focus { body: "3" })
        );
        assert!(classify("-- sqlly focus 2").is_none());
        assert!(classify("-- sqlly focused: 2").is_none());
        let diags = directive_diagnostics("-- sqlly focus: 0\nSELECT 1");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("focus"));
        assert!(directive_diagnostics("-- sqlly focus: 2\nSELECT 1").is_empty());
        assert_eq!(
            directive_diagnostics("SELECT 1 -- sqlly focus: 2").len(),
            1,
            "a trailing focus directive is ignored and says so"
        );
    }

    #[test]
    fn layout_and_lines_values_are_checked() {
        let diags = directive_diagnostics("-- sqlly layout: grid\nSELECT 1");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("Unknown layout"));
        let diags = directive_diagnostics("-- sqlly lines: 0\nSELECT 1");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("row count"));
        assert!(directive_diagnostics("-- sqlly lines: many\nSELECT 1").len() == 1);
        // Honored off the first line too.
        assert!(
            directive_diagnostics(
                "SELECT 1;\n-- sqlly layout: canvas\n-- sqlly lines 1: 5\nSELECT 2"
            )
            .is_empty()
        );
    }

    #[test]
    fn classify_set_requires_whitespace() {
        assert!(classify("-- sqlly settings").is_none());
        assert_eq!(
            classify("-- sqlly set a = 1"),
            Some(Directive::Set { body: "a = 1" })
        );
    }

    #[test]
    fn rejects_forms_the_real_parsers_reject() {
        for line in [
            // `query_format::parse` matches the literal `-- sqlly format:` —
            // the no-space dash form never parses, so no hover either.
            "--sqlly format: column=X; number=0",
            // Keyword prefixes that are not the keyword.
            "-- sqlly formatting: column=X",
            "-- sqlly named 1: X",
            // Missing colons / digits the parsers require.
            "-- sqlly format column=X",
            "-- sqlly view pivot",
            "-- sqlly name: Customers",
            "-- sqllyview: pivot",
            // Values the name parser rejects: a zero index, an empty label.
            "-- sqlly name 0: zero",
            "-- sqlly name 1:",
            "-- sqlly name 1:   ",
            // Extra dashes: every real parser wants exactly `--` before the
            // (possibly padded) namespace.
            "---sqlly view: pivot",
            "--- sqlly name 1: X",
        ] {
            let sql = format!("SELECT 1;\n{line}\nSELECT 2;");
            let offset = sql.find("--").unwrap() + 3;
            assert!(
                directive_span_at(&sql, offset).is_none(),
                "should reject: {line}"
            );
        }
    }

    #[test]
    fn classify_returns_the_family_payload() {
        assert_eq!(
            classify("-- sqlly format: column=A; number=2"),
            Some(Directive::Format {
                body: " column=A; number=2"
            })
        );
        assert_eq!(
            classify("-- sqlly view:pivot"),
            Some(Directive::View { body: "pivot" })
        );
        assert_eq!(
            classify("-- sqlly pivot: row:A; column:B; value:C:sum"),
            Some(Directive::Pivot {
                body: " row:A; column:B; value:C:sum"
            })
        );
        assert_eq!(
            classify("-- sqlly result: 1-2|2"),
            Some(Directive::Result { body: " 1-2|2" })
        );
        assert_eq!(
            classify("-- sqlly 4x2"),
            Some(Directive::Canvas {
                rows: 4,
                columns: 2
            })
        );
    }

    #[test]
    fn classify_name_reports_the_trimmed_label_span() {
        let comment = "-- sqlly name 2:   Open Orders  ";
        let Some(Directive::Name {
            number,
            name,
            name_start,
            name_end,
        }) = classify(comment)
        else {
            panic!("expected a name directive");
        };
        assert_eq!(number, 2);
        assert_eq!(name, "Open Orders");
        assert_eq!(&comment[name_start..name_end], "Open Orders");
    }

    #[test]
    fn classify_body_offsets_are_relative_to_the_body() {
        // `result_set_names` hands over the text after `--` — spans must be
        // relative to that string, two bytes earlier than via `classify`.
        let body = " sqlly name 1: Orders";
        let Some(Directive::Name {
            name_start,
            name_end,
            ..
        }) = classify_body(body)
        else {
            panic!("expected a name directive");
        };
        assert_eq!(&body[name_start..name_end], "Orders");
    }

    #[test]
    fn canvas_requires_plain_digits() {
        // A sign or an overflowing value is not a canvas anywhere — the
        // layout parser and the hover agree by construction.
        assert_eq!(classify("-- sqlly +2x2"), None);
        assert_eq!(classify("-- sqlly 2x-2"), None);
        assert_eq!(classify("-- sqlly 99999999999999999999x2"), None);
        // Whitespace inside the shape is tolerated, as it always was.
        assert_eq!(
            classify("-- sqlly 2 X 3"),
            Some(Directive::Canvas {
                rows: 2,
                columns: 3
            })
        );
    }

    #[test]
    fn keyword_span_only_covers_the_sqlly_lead_in() {
        let sql = "SELECT 1\n-- sqlly format: column=A; width=32dp";
        let start = sql.find("--").unwrap();
        let keyword_end = sql.find("sqlly").unwrap() + "sqlly".len();
        assert_eq!(
            directive_keyword_span_at(sql, start),
            Some((start, sql.len()))
        );
        assert!(directive_keyword_span_at(sql, keyword_end - 1).is_some());
        assert!(directive_keyword_span_at(sql, keyword_end).is_some());
        // Over the settings themselves: no popup.
        assert!(directive_keyword_span_at(sql, sql.find("column").unwrap()).is_none());
        assert!(directive_keyword_span_at(sql, sql.len()).is_none());
        // The full-line span still matches there (diagnostics, etc.).
        assert!(directive_span_at(sql, sql.find("column").unwrap()).is_some());
    }

    #[test]
    fn ignores_plain_comments_and_code() {
        let sql = "SELECT 1; -- just a note\nSELECT 2;";
        // Inside the plain comment.
        let note = sql.find("just").unwrap();
        assert!(directive_span_at(sql, note).is_none());
        // Inside the SELECT code.
        let code = sql.find("SELECT").unwrap();
        assert!(directive_span_at(sql, code).is_none());
    }

    #[test]
    fn directive_text_inside_string_literal_is_not_a_directive() {
        let sql = "SELECT '-- sqlly format: column=A; number=2' FROM t";
        let inside = sql.find("format").unwrap();
        assert!(directive_span_at(sql, inside).is_none());
    }

    #[test]
    fn string_containing_dashes_does_not_hide_the_real_comment() {
        let sql = "SELECT '--' + [a--b] -- sqlly view: pivot";
        let comment_start = sql.rfind("-- sqlly").unwrap();
        let (start, end) = directive_span_at(sql, comment_start + 4).unwrap();
        assert_eq!(start, comment_start);
        assert_eq!(end, sql.len());
    }

    #[test]
    fn escaped_quotes_keep_string_state_correct() {
        // The '' escape stays inside the string; the -- after it is still
        // string content, not a comment.
        let sql = "SELECT 'it''s -- sqlly view: pivot' FROM t";
        let inside = sql.find("view").unwrap();
        assert!(directive_span_at(sql, inside).is_none());
    }

    #[test]
    fn offset_off_a_char_boundary_does_not_panic() {
        let sql = "SELECT 'é' -- sqlly view: grid";
        // Probe every byte offset, including mid-UTF-8 positions.
        for offset in 0..=sql.len() {
            let _ = directive_span_at(sql, offset);
        }
    }

    #[test]
    fn detects_stacked_layout_directives() {
        for line in [
            "-- sqlly 2x2",
            "-- sqlly 10x1",
            "-- sqlly result: 1|1",
            "-- sqlly result: 1-2|2",
            // `result_layout` lowercases before splitting, so the uppercase X
            // form is a real directive and must hover too.
            "-- sqlly 2X2",
            "-- SQLLY 3x1",
        ] {
            let sql = format!("{line}\nSELECT 1;");
            let offset = sql.find("--").unwrap() + 3;
            assert!(
                directive_span_at(&sql, offset).is_some(),
                "should detect: {line}"
            );
        }
        // Not canvas shapes / placements.
        for line in [
            "-- sqlly 2x",
            "-- sqlly x2",
            "-- sqlly 2x2x2",
            "-- sqlly result 1|1",
        ] {
            let sql = format!("{line}\nSELECT 1;");
            let offset = sql.find("--").unwrap() + 3;
            assert!(
                directive_span_at(&sql, offset).is_none(),
                "should reject: {line}"
            );
        }
    }

    #[test]
    fn diagnostics_flag_unrecognized_sqlly_comments() {
        let sql = "SELECT 1;\n-- sqlly formt: column=A; number=2\nSELECT 2;";
        let diags = directive_diagnostics(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        let d = &diags[0];
        assert!(d.message.contains("Not a recognized SQLLY directive"));
        assert_eq!(&sql[d.start..d.end], "-- sqlly formt: column=A; number=2");
    }

    #[test]
    fn diagnostics_flag_directives_only_the_hover_used_to_accept() {
        // These named the namespace but no parser ever honored them; they
        // must squiggle rather than silently do nothing.
        for line in [
            "---sqlly view: pivot",
            "-- sqlly name 0: zero",
            "-- sqlly name 1:",
        ] {
            let sql = format!("SELECT 1;\n{line}\nSELECT 2;");
            let diags = directive_diagnostics(&sql);
            assert_eq!(diags.len(), 1, "{line}: {diags:?}");
            assert!(
                diags[0]
                    .message
                    .contains("Not a recognized SQLLY directive"),
                "{line}: {diags:?}"
            );
        }
    }

    #[test]
    fn diagnostics_call_out_the_no_space_format_spelling() {
        let sql = "--sqlly format: column=A; number=2\nSELECT 1;";
        let diags = directive_diagnostics(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("-- sqlly format:"), "{diags:?}");
    }

    #[test]
    fn diagnostics_flag_format_body_the_parser_rejects() {
        // Family prefix is right but there is no column= rule to apply.
        let sql = "-- sqlly format: number=2\nSELECT 1;";
        let diags = directive_diagnostics(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("column=<name>"), "{diags:?}");
    }

    #[test]
    fn diagnostics_flag_view_directive_off_the_first_line() {
        let sql = "SELECT 1;\n-- sqlly view: pivot\nSELECT 2;";
        let diags = directive_diagnostics(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("first non-empty line"),
            "{diags:?}"
        );
    }

    #[test]
    fn diagnostics_stay_quiet_on_valid_directives_and_plain_comments() {
        let sql = "-- sqlly view: pivot\n\
                   -- sqlly format: column=Total; number=2\n\
                   -- sqlly name 1: Customers\n\
                   -- sqlly pivot: row:Region; column:Year; value:Sales:sum\n\
                   -- just a note\n\
                   SELECT 'sqlly text -- sqlly nonsense' FROM t;";
        assert!(directive_diagnostics(sql).is_empty());
    }

    #[test]
    fn diagnostics_stay_quiet_on_stacked_layout_at_the_top() {
        let sql = "-- sqlly 2x2\nSELECT 1;\n-- sqlly result: 1|2\nSELECT 2;";
        assert!(directive_diagnostics(sql).is_empty());
    }

    #[test]
    fn diagnostics_flag_directives_trailing_sql_code() {
        // Every family's parser except `name` only honors comments that are
        // the whole line — a trailing directive is silently ignored, so it
        // must squiggle.
        for comment in [
            "-- sqlly format: column=Total; number=2",
            "-- sqlly view: pivot",
            "-- sqlly pivot: row:Region; column:Year; value:Sales:sum",
            "-- sqlly 2x2",
            "-- sqlly result: 1|1",
        ] {
            let sql = format!("SELECT 1 {comment}\nSELECT 2;");
            let diags = directive_diagnostics(&sql);
            assert_eq!(diags.len(), 1, "{comment}: {diags:?}");
            assert!(
                diags[0].message.contains("whole line"),
                "{comment}: {diags:?}"
            );
            assert_eq!(&sql[diags[0].start..diags[0].end], comment);
        }
    }

    /// The canvas-layout diagnostics only, with the text each one covers.
    fn layout_diags(sql: &str) -> Vec<(String, String)> {
        directive_diagnostics(sql)
            .into_iter()
            .filter(|d| d.message.starts_with("Result layout"))
            .map(|d| (sql[d.start..d.end].to_string(), d.message))
            .collect()
    }

    #[test]
    fn diagnostics_flag_overlapping_canvas_placements_on_their_comment() {
        let sql = "-- sqlly 2x2\n\
                   -- sqlly result: 1|1-2\n\
                   SELECT 1;\n\
                   \t-- sqlly result: 1|2  \n\
                   SELECT 2;";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].0, "-- sqlly result: 1|2");
        assert!(diags[0].1.contains("overlaps"), "{diags:?}");
    }

    #[test]
    fn diagnostics_flag_malformed_and_out_of_board_canvas_placements() {
        let sql = "-- sqlly 2x2\n\
                   -- sqlly result: 1\n\
                   SELECT 1;\n\
                   -- sqlly result: 3|1\n\
                   SELECT 2;";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].0, "-- sqlly result: 1");
        assert!(diags[0].1.contains("malformed"), "{diags:?}");
        assert_eq!(diags[1].0, "-- sqlly result: 3|1");
        assert!(diags[1].1.contains("outside canvas 2x2"), "{diags:?}");
    }

    #[test]
    fn diagnostics_flag_a_dangling_canvas_placement() {
        let sql = "-- sqlly 1x2\nSELECT 1;\n-- sqlly result: 1|2\nUPDATE t SET a = 1;";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].0, "-- sqlly result: 1|2");
        assert!(diags[0].1.contains("not followed by"), "{diags:?}");
    }

    #[test]
    fn diagnostics_flag_statements_the_board_has_no_cell_for() {
        // More result-producing statements than cells: an unplaced statement
        // is flagged on its own line.
        let sql = "-- sqlly 1x2\r\nSELECT 1;\r\nSELECT 2;\r\n  SELECT 3;  ";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].0, "SELECT 3;");
        assert!(
            diags[0].1.contains("no open cell for statement 3"),
            "{diags:?}"
        );
    }

    #[test]
    fn diagnostics_flag_a_placed_statement_left_without_a_cell_on_its_directive() {
        // The second placement overlaps the full-width first one; with no
        // cell left to auto-place it into, both problems land on its comment.
        let sql = "-- sqlly 1x2\n\
                   -- sqlly result: 1|1-2\n\
                   SELECT 1;\n\
                   -- sqlly result: 1|1\n\
                   SELECT 2;";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(
            diags.iter().all(|(text, _)| text == "-- sqlly result: 1|1"),
            "{diags:?}"
        );
        assert!(diags[0].1.contains("overlaps"), "{diags:?}");
        assert!(
            diags[1].1.contains("no open cell for statement 2"),
            "{diags:?}"
        );
    }

    #[test]
    fn diagnostics_flag_an_oversized_canvas_on_its_own_line() {
        let sql = "\n\n  -- sqlly 9x2\nSELECT 1;";
        let diags = layout_diags(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].0, "-- sqlly 9x2");
        assert!(diags[0].1.contains("exceeds maximum"), "{diags:?}");
    }

    #[test]
    fn diagnostics_stay_quiet_on_a_valid_canvas_layout() {
        let sql = "-- sqlly 2x4\n\
                   -- sqlly result: 1|1-4\n\
                   SELECT 1;\n\
                   -- sqlly result: 2|1-2\n\
                   SELECT 2;\n\
                   SELECT 3;";
        assert!(directive_diagnostics(sql).is_empty());
    }

    #[test]
    fn line_byte_ranges_split_every_line_ending_style() {
        let sql = "a\r\nbb\rc\n\nd";
        let lines: Vec<&str> = line_byte_ranges(sql)
            .into_iter()
            .map(|(start, end)| &sql[start..end])
            .collect();
        assert_eq!(lines, ["a", "bb", "c", "", "d"]);
    }

    #[test]
    fn diagnostics_stay_quiet_on_trailing_name_directives() {
        // The name scanner is SQL-aware and honors trailing comments.
        let sql = "SELECT 1 -- sqlly name 1: Users\nSELECT 2;";
        assert!(directive_diagnostics(sql).is_empty());
        assert_eq!(
            crate::result_set_names::parse(sql).len(),
            1,
            "the parser really does honor a trailing name comment"
        );
    }

    #[test]
    fn diagnostics_flag_unknown_view_values() {
        // The parser silently treats anything but `pivot` as grid.
        let sql = "-- sqlly view: card\nSELECT 1;";
        let diags = directive_diagnostics(sql);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("grid"), "{diags:?}");
        assert!(diags[0].message.contains("pivot"), "{diags:?}");
        for good in [
            "-- sqlly view: grid",
            "-- sqlly view: pivot",
            "-- sqlly view:PIVOT",
        ] {
            let sql = format!("{good}\nSELECT 1;");
            assert!(
                directive_diagnostics(&sql).is_empty(),
                "should stay quiet: {good}"
            );
        }
    }

    #[test]
    fn block_comment_on_the_line_shields_directive_text() {
        // A commented-out directive is not a directive: no hover, no squiggle
        // (previously it squiggled with a misleading format-body message).
        let sql = "/* -- sqlly format: column=A; number=2 */\nSELECT 1;";
        let inside = sql.find("format").unwrap();
        assert!(directive_span_at(sql, inside).is_none());
        assert!(directive_diagnostics(sql).is_empty());
        // Nested block comments stay shielded (T-SQL nests them).
        let nested = "/* a /* b */ -- sqlly view: pivot */\nSELECT 1;";
        assert!(directive_diagnostics(nested).is_empty());
        // A block comment that closes on the line does not hide a real
        // trailing comment after it.
        let closed = "SELECT /* note */ 1 -- sqlly name 1: X";
        let offset = closed.find("name").unwrap();
        assert!(directive_span_at(closed, offset).is_some());
    }

    /// The five drift fixes the lexer unification made, in one auditable
    /// table: forms no parser ever honored now classify nowhere, and the one
    /// form the parsers honored but hover missed now classifies everywhere.
    #[test]
    fn the_unification_drift_fixes_hold() {
        for rejected_everywhere in [
            "-- sqlly name 0: zero",
            "-- sqlly name 1:",
            "---sqlly view: pivot",
            "-- sqlly +2x2",
            "-- sqlly 99999999999999999999x2",
        ] {
            assert_eq!(
                classify(rejected_everywhere),
                None,
                "must not classify: {rejected_everywhere}"
            );
        }
        assert_eq!(
            classify("-- sqlly\t2x2"),
            Some(Directive::Canvas {
                rows: 2,
                columns: 2
            }),
            "tab after the namespace is accepted everywhere"
        );
    }

    #[test]
    fn names_namespace_tolerates_tabs_and_extra_dashes() {
        assert!(names_namespace("--\tsqlly view: pivot"));
        assert!(names_namespace("---sqlly view: pivot"));
        assert!(names_namespace("-- sqlly"));
        assert!(!names_namespace("-- sqllything"));
        assert!(!names_namespace("-- select sqlly"));
    }

    #[test]
    fn crlf_input_keeps_directives_quiet_and_classified() {
        // `\r` is ASCII whitespace; bodies tolerate it everywhere.
        assert_eq!(
            classify("-- sqlly 2x2\r"),
            Some(Directive::Canvas {
                rows: 2,
                columns: 2
            })
        );
        let sql = "-- sqlly view: pivot\r\n-- sqlly format: column=A; number=2\r\nSELECT 1;\r\n";
        assert!(directive_diagnostics(sql).is_empty());
    }

    #[test]
    fn strip_prefix_ci_is_char_boundary_safe() {
        // A multibyte char straddling the probe length must not panic and
        // must simply fail the match.
        assert_eq!(strip_prefix_ci("é", "e"), None);
        assert_eq!(strip_prefix_ci("-- SQLLY x", "-- sqlly "), Some("x"));
        assert_eq!(strip_prefix_ci("ab", "abc"), None);
    }

    #[test]
    fn span_covers_comment_only_not_leading_code() {
        let sql = "SELECT 1 -- sqlly format: column=A; number=2";
        let comment_start = sql.find("--").unwrap();
        // A point in the leading code is not part of the directive span.
        assert!(directive_span_at(sql, 0).is_none());
        // A point inside the comment resolves to a span starting at `--`.
        let (start, end) = directive_span_at(sql, comment_start + 5).unwrap();
        assert_eq!(start, comment_start);
        assert_eq!(end, sql.len());
    }

    #[test]
    fn date_markers_are_directives_honored_after_code() {
        assert_eq!(
            classify("--sqlly date:last wed at 3"),
            Some(Directive::Date {
                body: "last wed at 3"
            })
        );
        assert_eq!(
            classify("-- sqlly date: november").map(|d| d.kind()),
            Some(DirectiveKind::Date)
        );
        let sql =
            "SELECT *\nFROM Orders\nWHERE OrderDate >= '2026-10-01' --sqlly date:this month\n";
        assert!(directive_diagnostics(sql).is_empty());
        for doc in DIRECTIVES
            .iter()
            .filter(|d| d.kinds.contains(&DirectiveKind::Date))
        {
            for sample in doc.samples {
                assert_eq!(
                    sqlly_intel::natural_date::find_marked_predicates(sample).len(),
                    1,
                    "{sample}"
                );
            }
        }
    }

    /// Every grammar family the lexer can produce is documented in exactly
    /// one reference-window entry — no undocumented directives, ever.
    #[test]
    fn every_directive_kind_is_documented_exactly_once() {
        for kind in [
            DirectiveKind::Format,
            DirectiveKind::Name,
            DirectiveKind::View,
            DirectiveKind::Pivot,
            DirectiveKind::Chart,
            DirectiveKind::Result,
            DirectiveKind::Canvas,
            DirectiveKind::Focus,
            DirectiveKind::Date,
            DirectiveKind::Grid,
            DirectiveKind::Conditional,
            DirectiveKind::Column,
            DirectiveKind::Map,
        ] {
            let entries = DIRECTIVES
                .iter()
                .filter(|doc| doc.kinds.contains(&kind))
                .count();
            assert_eq!(entries, 1, "kind {kind:?} must have exactly one doc entry");
        }
    }

    /// Every worked sample in the reference window must classify — and
    /// classify to a family its own doc entry claims. The docs, the lexer,
    /// and (below) the parsers may never drift apart. Samples that don't
    /// start with `--` are option fragments, not whole directives.
    #[test]
    fn every_documented_sample_classifies_to_its_own_family() {
        for doc in DIRECTIVES {
            for sample in doc.samples {
                if !sample.starts_with("--") {
                    continue;
                }
                let directive = classify(sample)
                    .unwrap_or_else(|| panic!("documented sample must classify: {sample}"));
                assert!(
                    doc.kinds.contains(&directive.kind()),
                    "sample {sample:?} classifies as {:?}, not documented under {:?}",
                    directive.kind(),
                    doc.kinds
                );
                // As the first line of a query, a documented sample must never
                // squiggle.
                let sql = format!("{sample}\nSELECT 1;");
                assert!(
                    directive_diagnostics(&sql).is_empty(),
                    "documented sample must produce no diagnostics: {sample}"
                );
            }
        }
    }

    /// Each documented sample must also be honored by its family's real
    /// parser — not just match the lexer.
    #[test]
    fn documented_samples_are_honored_by_their_real_parsers() {
        for doc in DIRECTIVES {
            for sample in doc.samples {
                if !sample.starts_with("--") {
                    continue;
                }
                let kind = classify(sample).expect("sample classifies").kind();
                match kind {
                    DirectiveKind::Format => assert!(
                        !crate::query_format::parse(sample).is_empty(),
                        "format sample must yield a rule: {sample}"
                    ),
                    DirectiveKind::Name => assert_eq!(
                        crate::result_set_names::parse(&format!("{sample}\nSELECT 1;")).len(),
                        1,
                        "name sample must yield an assignment: {sample}"
                    ),
                    DirectiveKind::View => {
                        let hints =
                            crate::result_pivot::parse_view_hints(&format!("{sample}\nSELECT 1;"));
                        let expects_pivot = sample.to_ascii_lowercase().contains("pivot");
                        assert_eq!(
                            hints.default_view == crate::result_pivot::ResultViewKind::Pivot,
                            expects_pivot,
                            "view sample must set the view: {sample}"
                        );
                    }
                    DirectiveKind::Pivot => {
                        let hints =
                            crate::result_pivot::parse_view_hints(&format!("{sample}\nSELECT 1;"));
                        assert!(
                            hints.pivot.is_some(),
                            "pivot sample must yield a spec: {sample}"
                        );
                    }
                    DirectiveKind::Layout => {
                        let directives = crate::result_layout::parse_layout_directives(&format!(
                            "{sample}\nSELECT 1;"
                        ));
                        assert!(
                            directives.layout.is_some(),
                            "layout sample must pick a layout: {sample}"
                        );
                    }
                    DirectiveKind::Lines => {
                        let directives = crate::result_layout::parse_layout_directives(&format!(
                            "{sample}\nSELECT 1;"
                        ));
                        assert!(
                            directives.default_lines.is_some() || !directives.lines.is_empty(),
                            "lines sample must allot rows: {sample}"
                        );
                    }
                    DirectiveKind::Focus => {
                        let directives = crate::result_layout::parse_layout_directives(&format!(
                            "{sample}\nSELECT 1;"
                        ));
                        assert!(
                            directives.focus.is_some(),
                            "focus sample must pick a result set: {sample}"
                        );
                    }
                    DirectiveKind::Chart => {
                        let hints =
                            crate::result_pivot::parse_view_hints(&format!("{sample}\nSELECT 1;"));
                        assert!(
                            hints.chart.is_some(),
                            "chart sample must yield a spec: {sample}"
                        );
                    }
                    DirectiveKind::Canvas => {
                        let spec = crate::result_layout::parse(&format!("{sample}\nSELECT 1;"));
                        assert!(
                            spec.rows > 0 && spec.columns > 0,
                            "canvas sample must yield a canvas: {sample}"
                        );
                    }
                    DirectiveKind::Result => {
                        let sql = format!("-- sqlly 2x2\n{sample}\nSELECT 1;");
                        let spec = crate::result_layout::parse(&sql);
                        assert_eq!(
                            spec.placements.len(),
                            1,
                            "result sample must place a statement: {sample}"
                        );
                        assert!(
                            spec.diagnostics.is_empty(),
                            "result sample must place cleanly: {sample} — {:?}",
                            spec.diagnostics
                        );
                    }
                    DirectiveKind::Param => {
                        let specs = crate::query_params::parse_param_directives(&format!(
                            "{sample}\nSELECT 1;"
                        ));
                        assert_eq!(specs.len(), 1, "param sample must yield a spec: {sample}");
                    }
                    DirectiveKind::Set => {
                        let specs = crate::query_params::parse_set_directives(&format!(
                            "{sample}\nSELECT 1;"
                        ));
                        assert_eq!(specs.len(), 1, "set sample must yield a spec: {sample}");
                    }
                    DirectiveKind::Grid => {
                        let found = crate::grid_directive::parse(&format!("{sample}\nSELECT 1;"));
                        assert_eq!(found.len(), 1, "grid sample must yield settings: {sample}");
                    }
                    DirectiveKind::Column => {
                        let found = crate::computed_columns::parse(&format!("{sample}\nSELECT 1;"));
                        assert_eq!(
                            found.len(),
                            1,
                            "column sample must yield a column: {sample}"
                        );
                    }
                    DirectiveKind::Conditional => {
                        let found =
                            crate::conditional_directive::parse(&format!("{sample}\nSELECT 1;"));
                        assert_eq!(
                            found.len(),
                            1,
                            "conditional sample must yield a rule: {sample}"
                        );
                    }
                    DirectiveKind::Map => {
                        let found =
                            crate::map_view::directive::parse(&format!("{sample}\nSELECT 1;"));
                        assert_eq!(found.len(), 1, "map sample must yield settings: {sample}");
                        assert!(
                            !found[0].is_empty()
                                && crate::map_view::directive::diagnostics(
                                    sample.split_once(':').map(|(_, b)| b).unwrap_or("")
                                )
                                .is_empty(),
                            "map sample must parse cleanly: {sample}"
                        );
                    }
                    DirectiveKind::Date => {
                        let found = sqlly_intel::natural_date::find_marked_predicates(sample);
                        assert_eq!(
                            found.len(),
                            1,
                            "date sample must mark a predicate: {sample}"
                        );
                    }
                }
            }
        }
    }
}
