use super::*;

pub(super) fn expand_wildcard(
    sql: &str,
    range: Range<usize>,
    qualifier: Option<&str>,
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    table_refs: &[sqlly_services::TableRef],
) -> Option<(String, usize)> {
    let line_start = sql[..range.start].rfind('\n').map_or(0, |index| index + 1);
    let indent: String = sql[line_start..range.start]
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect();
    let continuation = format!("\n{indent}    ");

    if let Some(qualifier) = qualifier {
        let table_ref = table_refs.iter().find(|table_ref| {
            table_ref
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
                || table_ref.name.eq_ignore_ascii_case(qualifier)
        })?;
        let object = resolve_table_ref_object(snapshot, table_ref)?;
        let prefix = table_ref.alias.as_deref().unwrap_or(qualifier);
        let replacement = object_columns(object)
            .into_iter()
            .map(|column| format!("{prefix}.{}", sql_identifier(&column)))
            .collect::<Vec<_>>()
            .join(&format!(",{continuation}"));
        if replacement.is_empty() {
            return None;
        }
        let cursor = range.start + replacement.len();
        return Some((replace_sql_range(sql, range, &replacement), cursor));
    }

    let objects = table_refs
        .iter()
        .map(|table_ref| resolve_table_ref_object(snapshot, table_ref))
        .collect::<Option<Vec<_>>>()?;
    if objects.is_empty() {
        return None;
    }

    let mut used_aliases: Vec<String> = table_refs
        .iter()
        .filter_map(|table_ref| table_ref.alias.clone())
        .collect();
    let aliases: Vec<Option<String>> = table_refs
        .iter()
        .map(|table_ref| {
            table_ref.alias.clone().or_else(|| {
                (table_refs.len() > 1).then(|| {
                    let alias = unique_alias(&table_ref.name, &used_aliases);
                    used_aliases.push(alias.clone());
                    alias
                })
            })
        })
        .collect();

    let replacement = table_refs
        .iter()
        .zip(objects)
        .zip(&aliases)
        .flat_map(|((_, object), alias)| {
            object_columns(object).into_iter().map(move |column| {
                alias.as_ref().map_or_else(
                    || sql_identifier(&column),
                    |alias| format!("{alias}.{}", sql_identifier(&column)),
                )
            })
        })
        .collect::<Vec<_>>()
        .join(&format!(",{continuation}"));
    if replacement.is_empty() {
        return None;
    }

    let mut edits = vec![(range.clone(), replacement.clone())];
    if table_refs.len() > 1 {
        let declaration_ends = table_declaration_name_ends(sql);
        if declaration_ends.len() < table_refs.len() {
            return None;
        }
        for ((table_ref, alias), end) in table_refs.iter().zip(&aliases).zip(declaration_ends) {
            if table_ref.alias.is_some() {
                continue;
            }
            let alias = alias.as_ref()?;
            edits.extend(table_qualifier_edits(sql, table_ref, alias));
            edits.push((end..end, format!(" {alias}")));
        }
    }

    let inserted_before_wildcard = edits
        .iter()
        .filter(|(edit_range, _)| edit_range.end <= range.start)
        .map(|(edit_range, replacement)| replacement.len() as isize - edit_range.len() as isize)
        .sum::<isize>();
    let cursor = (range.start as isize + inserted_before_wildcard) as usize + replacement.len();
    Some((apply_sql_edits(sql, edits), cursor))
}

pub(super) fn resolve_table_ref_object<'a>(
    snapshot: &'a sqlly_lang::sqlmodel::ModelSnapshot,
    table_ref: &sqlly_services::TableRef,
) -> Option<&'a sqlly_lang::sqlmodel::ObjectEntry> {
    snapshot
        .objects()
        .find(|object| table_ref_matches_object(table_ref, object))
        .map(AsRef::as_ref)
}

pub(super) fn unique_alias(name: &str, used: &[String]) -> String {
    let base = name
        .chars()
        .find(|character| character.is_ascii_alphabetic())
        .unwrap_or('t')
        .to_ascii_lowercase()
        .to_string();
    let mut candidate = base.clone();
    let mut suffix = 2;
    while used
        .iter()
        .any(|used| used.eq_ignore_ascii_case(&candidate))
    {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    candidate
}

pub(super) fn replace_sql_range(sql: &str, range: Range<usize>, replacement: &str) -> String {
    let mut updated = String::with_capacity(sql.len() - range.len() + replacement.len());
    updated.push_str(&sql[..range.start]);
    updated.push_str(replacement);
    updated.push_str(&sql[range.end..]);
    updated
}

pub(super) fn apply_sql_edits(sql: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0.start));
    let mut updated = sql.to_string();
    for (range, replacement) in edits {
        updated.replace_range(range, &replacement);
    }
    updated
}

pub(super) fn table_declaration_name_ends(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut ends = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            index = skip_sql_quoted(bytes, index, b'\'');
            continue;
        }
        if bytes[index] == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if !bytes[index].is_ascii_alphabetic() {
            index += 1;
            continue;
        }
        let word_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        if !sql[word_start..index].eq_ignore_ascii_case("FROM")
            && !sql[word_start..index].eq_ignore_ascii_case("JOIN")
        {
            continue;
        }
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let object_start = index;
        while index < bytes.len() {
            match bytes[index] {
                b'[' => index = skip_sql_quoted(bytes, index, b']'),
                b'"' => index = skip_sql_quoted(bytes, index, b'"'),
                byte if byte.is_ascii_alphanumeric()
                    || matches!(byte, b'_' | b'$' | b'#' | b'.') =>
                {
                    index += 1;
                }
                _ => break,
            }
        }
        if index > object_start {
            ends.push(index);
        }
    }
    ends
}

pub(super) fn skip_sql_quoted(bytes: &[u8], start: usize, close: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == close {
            if close != b']' && bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

#[derive(Clone)]
pub(super) struct SqlIdentifierToken {
    pub(super) range: Range<usize>,
    pub(super) value: String,
}

pub(super) fn table_qualifier_edits(
    sql: &str,
    table_ref: &sqlly_services::TableRef,
    alias: &str,
) -> Vec<(Range<usize>, String)> {
    let tokens = sql_identifier_tokens(sql);
    let mut edits = Vec::new();
    for index in 0..tokens.len().saturating_sub(1) {
        let table = &tokens[index];
        let column = &tokens[index + 1];
        if !table.value.eq_ignore_ascii_case(&table_ref.name)
            || sql[table.range.end..column.range.start].trim() != "."
        {
            continue;
        }
        let start = if let (Some(schema), Some(previous)) = (
            table_ref.schema.as_deref(),
            index.checked_sub(1).map(|i| &tokens[i]),
        ) && previous.value.eq_ignore_ascii_case(schema)
            && sql[previous.range.end..table.range.start].trim() == "."
        {
            previous.range.start
        } else {
            table.range.start
        };
        edits.push((start..column.range.start, format!("{alias}.")));
    }
    edits
}

pub(super) fn sql_identifier_tokens(sql: &str) -> Vec<SqlIdentifierToken> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => index = skip_sql_quoted(bytes, index, b'\''),
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'[' | b'"' => {
                let start = index;
                let close = if bytes[index] == b'[' { b']' } else { b'"' };
                index = skip_sql_quoted(bytes, index, close);
                if index > start + 1 {
                    tokens.push(SqlIdentifierToken {
                        range: start..index,
                        value: sql[start + 1..index - 1].to_string(),
                    });
                }
            }
            byte if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$' | b'#') => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_identifier_byte(bytes[index]) {
                    index += 1;
                }
                tokens.push(SqlIdentifierToken {
                    range: start..index,
                    value: sql[start..index].to_string(),
                });
            }
            _ => index += 1,
        }
    }
    tokens
}

pub(super) fn wildcard_at(sql: &str, offset: usize) -> Option<(Range<usize>, Option<String>)> {
    let bytes = sql.as_bytes();
    let star = if bytes.get(offset) == Some(&b'*') {
        offset
    } else if offset > 0 && bytes.get(offset - 1) == Some(&b'*') {
        offset - 1
    } else {
        return None;
    };
    if star >= 2 && bytes.get(star - 1) == Some(&b'.') {
        let mut start = star - 1;
        while start > 0 && is_identifier_byte(bytes[start - 1]) {
            start -= 1;
        }
        let qualifier = sql[start..star - 1].to_string();
        (!qualifier.is_empty()).then_some((start..star + 1, Some(qualifier)))
    } else {
        Some((star..star + 1, None))
    }
}

pub(super) fn column_at(
    sql: &str,
    offset: usize,
) -> Option<(Range<usize>, String, Option<String>)> {
    if sql.is_empty() {
        return None;
    }
    let mut position = offset.min(sql.len().saturating_sub(1));
    while position > 0 && !sql.is_char_boundary(position) {
        position -= 1;
    }
    let (start, end) = token_range_at(sql, position);
    let column = sql.get(start..end)?.trim_matches(['[', ']', '`', '"']);
    if column.is_empty() || !column.bytes().all(is_identifier_byte) {
        return None;
    }
    let qualifier = if start > 1 && sql.as_bytes().get(start - 1) == Some(&b'.') {
        let mut qualifier_start = start - 1;
        while qualifier_start > 0 && is_identifier_byte(sql.as_bytes()[qualifier_start - 1]) {
            qualifier_start -= 1;
        }
        Some(sql[qualifier_start..start - 1].to_string())
    } else {
        None
    };
    Some((start..end, column.to_string(), qualifier))
}

/// Table refs in scope that own a column named `column` (case-insensitive).
pub(super) fn column_owners<'a>(
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    table_refs: &'a [sqlly_services::TableRef],
    column: &str,
) -> Vec<&'a sqlly_services::TableRef> {
    table_refs
        .iter()
        .filter(|table_ref| {
            snapshot.objects().any(|object| {
                table_ref_matches_object(table_ref, object)
                    && object_columns(object)
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(column))
            })
        })
        .collect()
}

/// The prefix a column of `table_ref` is written with: its alias, else its name.
/// Wrapping brackets or quotes are stripped so [`sql_identifier`] can re-quote.
pub(super) fn qualifier_for(table_ref: &sqlly_services::TableRef) -> String {
    let raw = table_ref
        .alias
        .as_deref()
        .unwrap_or(table_ref.name.as_str());
    raw.trim_matches(['[', ']', '`', '"']).to_string()
}

/// [`qualifier_for`] plus recovery when `CompletionContext` truncated a
/// bracketed alias at a space (`[order line]` arrives as `[order`).
pub(super) fn qualifier_spelling(sql: &str, table_ref: &sqlly_services::TableRef) -> String {
    let stripped = qualifier_for(table_ref);
    if table_ref
        .alias
        .as_deref()
        .is_some_and(|alias| alias.starts_with('[') && !alias.contains(']'))
    {
        let prefix = stripped.as_str();
        if let Some(token) = sql_identifier_tokens(sql).into_iter().find(|token| {
            token.value.len() > prefix.len()
                && token.value[..prefix.len()].eq_ignore_ascii_case(prefix)
                && token.value[prefix.len()..].starts_with(' ')
        }) {
            return token.value;
        }
    }
    stripped
}

/// `col` → `q.col`; returns the new SQL and the cursor placed after the column.
pub(super) fn qualify_identifier(
    sql: &str,
    range: Range<usize>,
    qualifier: &str,
) -> (String, usize) {
    let prefix = format!("{}.", sql_identifier(qualifier));
    (
        replace_sql_range(sql, range.start..range.start, &prefix),
        range.end + prefix.len(),
    )
}

/// `q.col` → `col`. `qualifier` is the exact text `column_at` returned so the
/// removed span is `qualifier.len() + 1` bytes ending at `range.start`.
pub(super) fn unqualify_identifier(
    sql: &str,
    range: Range<usize>,
    qualifier: &str,
) -> (String, usize) {
    let start = range.start.saturating_sub(qualifier.len() + 1);
    (
        replace_sql_range(sql, start..range.start, ""),
        range.end - qualifier.len() - 1,
    )
}

/// Qualify every unqualified identifier inside `selection` that exactly one
/// in-scope table owns. Returns the new SQL and how many identifiers changed;
/// `None` when nothing changed.
pub(super) fn qualify_all_in_range(
    sql: &str,
    selection: Range<usize>,
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    table_refs: &[sqlly_services::TableRef],
) -> Option<(String, usize)> {
    let mut edits = Vec::new();
    for token in sql_identifier_tokens(sql) {
        if token.range.start < selection.start || token.range.end > selection.end {
            continue;
        }
        let before = sql[..token.range.start].trim_end();
        let after = sql[token.range.end..].trim_start();
        if before.ends_with('.') || after.starts_with('.') || after.starts_with('(') {
            continue;
        }
        if table_refs.iter().any(|table_ref| {
            table_ref.name.eq_ignore_ascii_case(&token.value)
                || table_ref
                    .alias
                    .as_deref()
                    .is_some_and(|alias| alias.eq_ignore_ascii_case(&token.value))
        }) {
            continue;
        }
        let owners = column_owners(snapshot, table_refs, &token.value);
        if let [owner] = owners.as_slice() {
            edits.push((
                token.range.start..token.range.start,
                format!("{}.", sql_identifier(&qualifier_spelling(sql, owner))),
            ));
        }
    }
    if edits.is_empty() {
        return None;
    }
    let count = edits.len();
    Some((apply_sql_edits(sql, edits), count))
}

pub(super) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#')
}

pub(super) fn resolve_table_ref<'a>(
    snapshot: &'a sqlly_lang::sqlmodel::ModelSnapshot,
    table_refs: &[sqlly_services::TableRef],
    qualifier: Option<&str>,
) -> Option<&'a sqlly_lang::sqlmodel::ObjectEntry> {
    let table_ref = match qualifier {
        Some(qualifier) => table_refs.iter().find(|table_ref| {
            table_ref
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
                || table_ref.name.eq_ignore_ascii_case(qualifier)
        }),
        None if table_refs.len() == 1 => table_refs.first(),
        None => None,
    }?;
    snapshot
        .objects()
        .find(|object| table_ref_matches_object(table_ref, object))
        .map(AsRef::as_ref)
}

pub(super) fn table_ref_matches_object(
    table_ref: &sqlly_services::TableRef,
    object: &sqlly_lang::sqlmodel::ObjectEntry,
) -> bool {
    object
        .reference
        .name
        .spelling()
        .eq_ignore_ascii_case(&table_ref.name)
        && table_ref.schema.as_deref().is_none_or(|schema| {
            object
                .reference
                .namespace
                .as_ref()
                .is_some_and(|namespace| namespace.spelling().eq_ignore_ascii_case(schema))
        })
}

pub(super) fn object_columns(object: &sqlly_lang::sqlmodel::ObjectEntry) -> Vec<String> {
    match &object.body {
        sqlly_lang::sqlmodel::ObjectBody::Table(table) => table
            .columns
            .iter()
            .map(|column| column.name.spelling().to_string())
            .collect(),
        sqlly_lang::sqlmodel::ObjectBody::View(view) => view
            .columns
            .iter()
            .map(|column| column.name.spelling().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn object_column<'a>(
    object: &'a sqlly_lang::sqlmodel::ObjectEntry,
    name: &str,
) -> Option<&'a sqlly_lang::sqlmodel::ColumnDef> {
    let sqlly_lang::sqlmodel::ObjectBody::Table(table) = &object.body else {
        return None;
    };
    table
        .columns
        .iter()
        .find(|column| column.name.spelling().eq_ignore_ascii_case(name))
}

pub(super) fn unique_join_alias(name: &str, table_refs: &[sqlly_services::TableRef]) -> String {
    let base = name
        .chars()
        .find(|character| character.is_ascii_alphabetic())
        .unwrap_or('t')
        .to_ascii_lowercase()
        .to_string();
    let mut candidate = base.clone();
    let mut suffix = 2;
    while table_refs.iter().any(|table_ref| {
        table_ref
            .alias
            .as_deref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(&candidate))
    }) {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    candidate
}

/// Kinds that name a dotted object path and can be advanced segment by
/// segment. Keywords, snippets, join clauses, parameters, result-set columns,
/// and value-lookup items are never partially accepted this way.
pub(super) fn is_dot_advanceable_kind(kind: CompletionKind) -> bool {
    matches!(
        kind,
        CompletionKind::Database
            | CompletionKind::Schema
            | CompletionKind::Table
            | CompletionKind::Column
            | CompletionKind::Function
            | CompletionKind::Procedure
            | CompletionKind::Type
    )
}

/// Kinds that always have children below them in a dotted path. When the
/// proposed segment is the candidate's last label segment, only these
/// container kinds may still offer it (a leaf column or procedure has nothing
/// below the dot).
pub(super) fn is_dot_container_kind(kind: CompletionKind) -> bool {
    matches!(
        kind,
        CompletionKind::Database | CompletionKind::Schema | CompletionKind::Table
    )
}

/// The next dotted-path segment `item` would advance to, if any. `committed`
/// holds the qualifier segments already typed (e.g. `["Accounting"]` at
/// `Accounting.Acc|`) and `partial` is the segment fragment being typed.
///
/// The item's label is its display path (`Accounting.Account`). Because the
/// engine emits labels at the depth the user typed (`db.schema.` lists
/// `schema.table` labels), the label is aligned by suffix: the largest `j`
/// whose leading `j` label segments match the last `j` committed segments
/// case-insensitively wins, and the segment right after the match is the
/// proposal. An empty `committed` aligns at `j = 0`; a non-empty one requires
/// `j >= 1` so unrelated single-segment labels can't propose a sideways hop.
pub(super) fn dot_advance_segment<'a>(
    item: &'a CompletionItem,
    committed: &[&str],
    partial: &str,
) -> Option<&'a str> {
    if !is_dot_advanceable_kind(item.kind) {
        return None;
    }
    let segments: Vec<&str> = item
        .label
        .split('.')
        .map(|segment| segment.trim_matches(|ch| matches!(ch, '[' | ']' | '"' | '`')))
        .collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let min_alignment = usize::from(!committed.is_empty());
    let max_alignment = committed.len().min(segments.len() - 1);
    let alignment = (min_alignment..=max_alignment).rev().find(|&j| {
        let tail = &committed[committed.len() - j..];
        segments[..j]
            .iter()
            .zip(tail)
            .all(|(label, typed)| label.eq_ignore_ascii_case(typed))
    })?;
    let next = segments[alignment];
    let is_leaf_segment = alignment + 1 == segments.len();
    if is_leaf_segment && !is_dot_container_kind(item.kind) {
        return None;
    }
    if !partial.is_empty()
        && !next
            .get(..partial.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(partial))
    {
        return None;
    }
    Some(next)
}

/// Decide which segment `.` / Right-arrow should commit while the completion
/// popup is open. Returns the canonical (catalog-cased) next segment — without
/// the trailing dot — when every eligible visible candidate agrees on it, or,
/// after the user has manually navigated the popup, when the selected
/// candidate proposes one. Returns `None` when the candidates are ambiguous,
/// so the caller falls through to its normal behavior instead of guessing.
pub(super) fn decide_dot_advance(
    items: &[CompletionItem],
    selected_index: usize,
    user_navigated: bool,
    committed: &[&str],
    partial: &str,
) -> Option<String> {
    let mut unique: Vec<&str> = Vec::new();
    for item in items {
        if let Some(segment) = dot_advance_segment(item, committed, partial)
            && !unique.iter().any(|seen| seen.eq_ignore_ascii_case(segment))
        {
            unique.push(segment);
        }
    }
    match unique.as_slice() {
        [only] => Some((*only).to_string()),
        _ if user_navigated => items
            .get(selected_index)
            .and_then(|item| dot_advance_segment(item, committed, partial))
            .map(str::to_string),
        _ => None,
    }
}

pub(super) fn alias_already_follows(sql: &str, cursor: usize) -> bool {
    let bytes = sql.as_bytes();
    let mut idx = cursor.min(bytes.len());
    while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t') {
        idx += 1;
    }
    if idx >= bytes.len() || bytes[idx] == b'\n' || bytes[idx] == b'\r' {
        return false;
    }
    if bytes[idx..].starts_with(b"AS")
        && !bytes
            .get(idx + 2)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        let mut after_as = idx + 2;
        while after_as < bytes.len() && matches!(bytes[after_as], b' ' | b'\t') {
            after_as += 1;
        }
        let start = after_as;
        while after_as < bytes.len() && is_alias_char(bytes[after_as]) {
            after_as += 1;
        }
        return after_as > start;
    }
    let start = idx;
    while idx < bytes.len() && is_alias_char(bytes[idx]) {
        idx += 1;
    }
    if idx == start {
        return false;
    }
    let word = &sql[start..idx];
    !sqlly_intel::intellisense::is_reserved_alias_word(word)
}

fn is_alias_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'@' | b'#')
}

/// The unqualified leaf segment of a completion's dotted label, with SQL
/// identifier bracketing characters stripped (`[dbo].[Customers]` → `Customers`).
pub(super) fn table_leaf_name(item: &CompletionItem) -> &str {
    item.label
        .rsplit('.')
        .next()
        .unwrap_or(item.label.as_str())
        .trim_matches(|character| matches!(character, '[' | ']' | '"' | '`'))
}

/// Like [`unique_join_alias`]' collision check, but the candidate must also
/// avoid the referenced table names themselves, so a capitals-derived alias
/// can never shadow a table the query already mentions.
fn unique_table_alias(base: &str, table_refs: &[sqlly_services::TableRef]) -> String {
    let is_taken = |candidate: &str| {
        table_refs.iter().any(|table_ref| {
            table_ref
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(candidate))
                || table_ref.name.eq_ignore_ascii_case(candidate)
        })
    };
    let mut candidate = base.to_string();
    let mut suffix = 2;
    while is_taken(&candidate) {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    candidate
}

/// Whether the cursor sits where a table source is being completed, i.e. the
/// position where an alias would be meaningful right after the table name.
fn is_table_source_intent(intent: sqlly_services::IntelliSenseIntent) -> bool {
    matches!(
        intent,
        sqlly_services::IntelliSenseIntent::FromSource
            | sqlly_services::IntelliSenseIntent::JoinSource
    )
}

/// The completion context for a table-source position, or `None` when the
/// caret is not completing a table after FROM or JOIN. A schema-qualified
/// prefix (`from dbo.su▏`) reads as a qualified member at the caret, so the
/// context is taken where the dotted name starts (feedback ifb_3edd).
fn table_source_context(sql: &str, cursor: usize) -> Option<sqlly_services::CompletionContext> {
    let context = sqlly_services::CompletionContext::auto(sql, cursor);
    if is_table_source_intent(context.intent) {
        return Some(context);
    }
    let name_start = sql[..cursor]
        .rfind(|c: char| {
            !(c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '"' | '`' | '$' | '#'))
        })
        .map_or(0, |i| i + sql[i..].chars().next().map_or(1, char::len_utf8));
    if name_start == cursor || !sql[name_start..cursor].contains('.') {
        return None;
    }
    let context = sqlly_services::CompletionContext::auto(sql, name_start);
    is_table_source_intent(context.intent).then_some(context)
}

/// The alias to offer as a one-tap follow-up suggestion right after a
/// table/view completion is accepted, when the user chose to confirm aliases
/// with a second acceptance rather than have them added automatically.
/// Returns the alias and the table leaf name for the suggestion's detail line.
pub(super) fn suggested_table_alias(
    item: &CompletionItem,
    preferences: &EditorPreferences,
    sql: &str,
    cursor: usize,
) -> Option<(String, String)> {
    if item.kind != CompletionKind::Table
        || item.detail == sqlly_intel::intellisense::FROM_CLAUSE_DETAIL
        || preferences.auto_add_table_alias
    {
        return None;
    }
    let context = table_source_context(sql, cursor)?;
    if alias_already_follows(sql, cursor) {
        return None;
    }
    let base = preferences.alias_suggestion(&item.label)?;
    let alias = unique_table_alias(&base, &context.table_refs);
    Some((alias, table_leaf_name(item).to_string()))
}

/// Apply the optional alias only when a plain table/view completion is
/// accepted. Keeping this out of completion construction means the typing
/// hot path and its ranking stay unchanged.
pub(super) fn accepted_completion_insert_text(
    item: &CompletionItem,
    preferences: &EditorPreferences,
    sql: &str,
    cursor: usize,
) -> String {
    // A table offered after a finished SELECT list writes its own `FROM`
    // clause, and a column offered after a FROM source its own `WHERE`;
    // rebuild either with the user's indent unit rather than the one the
    // engine inferred from the document.
    if let Some(keyword) = sqlly_intel::intellisense::clause_keyword_for_detail(&item.detail) {
        let replace_start =
            cursor.saturating_sub(sqlly_intel::intellisense::current_prefix(sql, cursor).len());
        return sqlly_intel::intellisense::clause_insert_text(
            sql,
            replace_start,
            keyword,
            clause_insert_name(&item.insert_text),
            &preferences.indent_unit(),
        );
    }
    if item.kind != CompletionKind::Table || !preferences.auto_add_table_alias {
        return item.insert_text.clone();
    }
    let style = match preferences.alias_generation {
        AliasGeneration::DoNotSuggest => return item.insert_text.clone(),
        AliasGeneration::CapitalLettersLowercase => {
            sqlly_intel::intellisense::AliasStyle::CapitalLetters {
                include_schema: preferences.include_schema_in_alias,
            }
        }
        AliasGeneration::AutoBasedOnJoin => sqlly_intel::intellisense::AliasStyle::FirstLetter,
    };
    let Some(context) = table_source_context(sql, cursor) else {
        return item.insert_text.clone();
    };
    if alias_already_follows(sql, cursor) {
        return item.insert_text.clone();
    }
    let existing: Vec<String> = context
        .table_refs
        .iter()
        .filter_map(|table_ref| table_ref.alias.clone())
        .collect();
    let name = item
        .label
        .trim_matches(|character| matches!(character, '[' | ']' | '"' | '`'));
    match sqlly_intel::intellisense::suggest_alias(name, style, &existing) {
        Some(alias) => preferences.aliased_table(&item.insert_text, &alias),
        None => item.insert_text.clone(),
    }
}

/// The name a clause-writing insert (`FROM⏎⇥name`) ends with — already
/// quoted for the engine — so the clause can be rebuilt with the user's
/// indent unit without losing that quoting.
fn clause_insert_name(insert_text: &str) -> &str {
    let last_line = insert_text.rsplit('\n').next().unwrap_or(insert_text);
    last_line.trim_start_matches([' ', '\t'])
}

pub(super) fn insert_join_clause(sql: &str, join: &str) -> String {
    let insertion = top_level_clause_start(sql).unwrap_or(sql.trim_end_matches(';').len());
    let mut updated = String::with_capacity(sql.len() + join.len() + 2);
    updated.push_str(sql[..insertion].trim_end());
    updated.push('\n');
    updated.push_str(join);
    updated.push('\n');
    updated.push_str(sql[insertion..].trim_start());
    updated
}

pub(super) fn top_level_clause_start(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut depth = 0_u32;
    let mut saw_from = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'[' => {
                let close = if bytes[index] == b'[' {
                    b']'
                } else {
                    bytes[index]
                };
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == close {
                        if close != b']' && bytes.get(index + 1) == Some(&close) {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            byte if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                let word = sql[start..index].to_ascii_uppercase();
                if word == "FROM" {
                    saw_from = true;
                } else if saw_from
                    && matches!(
                        word.as_str(),
                        "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET"
                    )
                {
                    return Some(start);
                }
            }
            _ => index += 1,
        }
    }
    None
}

pub(super) fn sql_identifier(name: &str) -> String {
    if name
        .bytes()
        .enumerate()
        .all(|(index, byte)| is_identifier_byte(byte) && (index > 0 || !byte.is_ascii_digit()))
    {
        name.to_string()
    } else {
        format!("[{}]", name.replace(']', "]]"))
    }
}

/// Complete SQLLY's structured directives without sending comment text through
/// the SQL completion engine.
pub(super) fn pivot_directive_completions(
    content: &str,
    cursor: usize,
) -> Option<(Vec<CompletionItem>, usize)> {
    let cursor = cursor.min(content.len());
    let line_start = content[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line = &content[line_start..cursor];
    let trimmed = line.trim_start();
    if !trimmed.starts_with("--") {
        return None;
    }

    let replace_start = line_start + line.len() - trimmed.len();
    let item = |label: &str, insert_text: &str, detail: &str| CompletionItem {
        label: label.to_string(),
        insert_text: insert_text.to_string(),
        kind: CompletionKind::Snippet,
        source: CompletionSource::Snippet,
        score: 10_000,
        detail: detail.to_string(),
        replace_span: Some((replace_start, cursor)),
    };

    // Trigger recognition rides the shared lexer, so every spelling the
    // grammar accepts (`--sqlly`, tabs, any case) offers the same menu; the
    // family menu shows while the namespace itself is still being typed.
    use crate::sqlly_directive::Directive;
    let items = match crate::sqlly_directive::classify(trimmed) {
        Some(Directive::Pivot { .. }) => vec![item(
            "Pivot directive",
            "-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum",
            "Separate groups with semicolons and row/column fields with commas; value names one aggregate measure.",
        )],
        Some(Directive::Format { .. }) => vec![
            item(
                "Number format",
                "-- sqlly format: column=Amount; number=2; thousands=true",
                "Format a numeric result column by name.",
            ),
            item(
                "Date format",
                "-- sqlly format: column=CreatedAt; date=%Y-%m-%d %H:%M:%S",
                "Format a date or timestamp result column by name.",
            ),
            item(
                "Boolean format",
                "-- sqlly format: column=Enabled; boolean=Yes/No",
                "Set the true and false labels for a boolean result column.",
            ),
            item(
                "Binary format",
                "-- sqlly format: column=Payload; binary=hex",
                "Render binary values as hexadecimal, base64, UUID or ULID.",
            ),
        ],
        Some(Directive::View { .. }) => vec![
            item(
                "Pivot result view",
                "-- sqlly view:pivot",
                "Open the interactive pivot tab by default.",
            ),
            item(
                "Grid result view",
                "-- sqlly view:grid",
                "Open the standard result grid by default.",
            ),
        ],
        // A complete name/canvas/placement directive needs no snippet menu.
        Some(_) => return None,
        None => {
            // Not (yet) a directive: offer the family menu while the comment
            // is only dashes or the bare namespace.
            let rest = trimmed[2..].trim();
            if !rest.is_empty() && !rest.eq_ignore_ascii_case("sqlly") {
                return None;
            }
            vec![
                item(
                    "Pivot result view",
                    "-- sqlly view:pivot\n-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum",
                    "Show the interactive pivot tab; separate groups with semicolons and fields with commas.",
                ),
                item(
                    "Grid result view",
                    "-- sqlly view:grid",
                    "Show the standard result grid.",
                ),
                item(
                    "Column format",
                    "-- sqlly format: column=Amount; number=2; thousands=true",
                    "Override result formatting by column name.",
                ),
                item(
                    "Result set name",
                    "-- sqlly name 1: Result name",
                    "Name a result set by its one-based position.",
                ),
                item(
                    "Stacked result layout",
                    "-- sqlly 2x2\n-- sqlly result: 1|1",
                    "Define a result canvas and place the following result-producing statement.",
                ),
            ]
        }
    };
    Some((items, replace_start))
}

/// Keywords that terminate a SELECT column list (i.e. clauses that come after
/// the projected columns). Used by `in_select_column_context`.
pub(super) const SELECT_TERMINATORS: &[&str] = &[
    "from",
    "where",
    "group",
    "order",
    "having",
    "into",
    "union",
    "except",
    "intersect",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListClause {
    Select,
    GroupBy,
    OrderBy,
}

impl ListClause {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Select => 0,
            Self::GroupBy => 1,
            Self::OrderBy => 2,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::GroupBy => "GROUP BY",
            Self::OrderBy => "ORDER BY",
        }
    }
}

pub(super) fn clause_header_only(line: &str, clause: ListClause) -> bool {
    let normalized = line
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    match clause {
        ListClause::Select => normalized == "select",
        ListClause::GroupBy => normalized.ends_with("group by"),
        ListClause::OrderBy => normalized.ends_with("order by"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SqlLexState {
    Code,
    SingleQuote,
    DoubleQuote,
    Bracket,
    Backtick,
    LineComment,
    BlockComment(usize),
}

pub(super) fn is_sql_code_at(sql: &str, target: usize) -> bool {
    let bytes = sql.as_bytes();
    let mut state = SqlLexState::Code;
    let mut index = 0usize;
    while index <= target.min(bytes.len().saturating_sub(1)) {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        match state {
            SqlLexState::Code => {
                if index == target {
                    return !matches!(
                        (byte, next),
                        (b'\'' | b'"' | b'[' | b'`', _) | (b'-', Some(b'-')) | (b'/', Some(b'*'))
                    );
                }
                match (byte, next) {
                    (b'-', Some(b'-')) => {
                        state = SqlLexState::LineComment;
                        index += 1;
                    }
                    (b'/', Some(b'*')) => {
                        state = SqlLexState::BlockComment(1);
                        index += 1;
                    }
                    (b'\'', _) => state = SqlLexState::SingleQuote,
                    (b'"', _) => state = SqlLexState::DoubleQuote,
                    (b'[', _) => state = SqlLexState::Bracket,
                    (b'`', _) => state = SqlLexState::Backtick,
                    _ => {}
                }
            }
            SqlLexState::SingleQuote => {
                if byte == b'\'' {
                    if next == Some(b'\'') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
            }
            SqlLexState::DoubleQuote => {
                if byte == b'"' {
                    if next == Some(b'"') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
            }
            SqlLexState::Bracket => {
                if byte == b']' {
                    if next == Some(b']') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
            }
            SqlLexState::Backtick => {
                if byte == b'`' {
                    if next == Some(b'`') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
            }
            SqlLexState::LineComment => {
                if byte == b'\n' {
                    state = SqlLexState::Code;
                }
            }
            SqlLexState::BlockComment(depth) => match (byte, next) {
                (b'/', Some(b'*')) => {
                    state = SqlLexState::BlockComment(depth + 1);
                    index += 1;
                }
                (b'*', Some(b'/')) => {
                    state = if depth == 1 {
                        SqlLexState::Code
                    } else {
                        SqlLexState::BlockComment(depth - 1)
                    };
                    index += 1;
                }
                _ => {}
            },
        }
        if index == target && state != SqlLexState::Code {
            return false;
        }
        index += 1;
    }
    state == SqlLexState::Code
}

/// Active top-level comma-separated clause at `cursor`, ignoring strings,
/// quoted identifiers, and comments. Returning `None` on uncertainty is the
/// essential safety property for the opt-in repair.
/// Whether a predicate line still expects more before a connector would make
/// sense: it ends in a connector or comparison word, or carries a `BETWEEN`
/// still waiting for its `AND`.
pub(super) fn predicate_line_is_incomplete(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    let last_word = lower
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|word| !word.is_empty())
        .unwrap_or("");
    if matches!(
        last_word,
        "and" | "or" | "on" | "where" | "not" | "in" | "like" | "between" | "is" | "exists"
    ) {
        return true;
    }
    // `x BETWEEN 1` on one line: the next line legitimately starts with AND,
    // which the user must type themselves.
    let betweens = lower.split_whitespace().filter(|w| *w == "between").count();
    let ands = lower.split_whitespace().filter(|w| *w == "and").count();
    betweens > ands
}

/// Whether `cursor` sits inside a predicate context — a `WHERE` clause or a
/// `JOIN … ON` — at parenthesis depth 0, using the same string/comment-aware
/// scan as [`list_clause_at`]. Returns `None` in every other clause.
pub(super) fn predicate_clause_at(sql: &str, cursor: usize) -> Option<()> {
    let end = cursor.min(sql.len());
    let bytes = sql.as_bytes();
    let mut state = SqlLexState::Code;
    let mut depth = 0i32;
    let mut in_predicate = false;
    let mut saw_join = false;
    let mut index = 0usize;
    while index < end {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if state != SqlLexState::Code {
            match state {
                SqlLexState::SingleQuote if byte == b'\'' => {
                    if next == Some(b'\'') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::DoubleQuote if byte == b'"' => {
                    if next == Some(b'"') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::Bracket if byte == b']' => {
                    if next == Some(b']') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::Backtick if byte == b'`' => {
                    if next == Some(b'`') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::LineComment if byte == b'\n' => state = SqlLexState::Code,
                SqlLexState::BlockComment(comment_depth) => match (byte, next) {
                    (b'/', Some(b'*')) => {
                        state = SqlLexState::BlockComment(comment_depth + 1);
                        index += 1;
                    }
                    (b'*', Some(b'/')) => {
                        state = if comment_depth == 1 {
                            SqlLexState::Code
                        } else {
                            SqlLexState::BlockComment(comment_depth - 1)
                        };
                        index += 1;
                    }
                    _ => {}
                },
                _ => {}
            }
            index += 1;
            continue;
        }
        match (byte, next) {
            (b'-', Some(b'-')) => {
                state = SqlLexState::LineComment;
                index += 2;
                continue;
            }
            (b'/', Some(b'*')) => {
                state = SqlLexState::BlockComment(1);
                index += 2;
                continue;
            }
            (b'\'', _) => state = SqlLexState::SingleQuote,
            (b'"', _) => state = SqlLexState::DoubleQuote,
            (b'[', _) => state = SqlLexState::Bracket,
            (b'`', _) => state = SqlLexState::Backtick,
            (b'(', _) => depth += 1,
            (b')', _) => depth -= 1,
            _ if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                let start = index;
                index += 1;
                while index < end && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                let word = sql[start..index].to_ascii_lowercase();
                match word.as_str() {
                    "where" => in_predicate = true,
                    "join" => {
                        saw_join = true;
                        in_predicate = false;
                    }
                    "on" => {
                        if saw_join {
                            in_predicate = true;
                        }
                    }
                    "select" | "from" | "group" | "order" | "having" | "union" | "except"
                    | "intersect" | "limit" | "offset" | "fetch" | "insert" | "update"
                    | "delete" | "set" | "values" => {
                        in_predicate = false;
                        if word != "from" {
                            saw_join = false;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    (state == SqlLexState::Code && depth == 0 && in_predicate).then_some(())
}

pub(super) fn list_clause_at(sql: &str, cursor: usize) -> Option<ListClause> {
    let end = cursor.min(sql.len());
    let bytes = sql.as_bytes();
    let mut state = SqlLexState::Code;
    let mut depth = 0i32;
    let mut active = None;
    let mut pending_by: Option<ListClause> = None;
    let mut index = 0usize;
    while index < end {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if state != SqlLexState::Code {
            match state {
                SqlLexState::SingleQuote if byte == b'\'' => {
                    if next == Some(b'\'') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::DoubleQuote if byte == b'"' => {
                    if next == Some(b'"') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::Bracket if byte == b']' => {
                    if next == Some(b']') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::Backtick if byte == b'`' => {
                    if next == Some(b'`') {
                        index += 1;
                    } else {
                        state = SqlLexState::Code;
                    }
                }
                SqlLexState::LineComment if byte == b'\n' => state = SqlLexState::Code,
                SqlLexState::BlockComment(comment_depth) => match (byte, next) {
                    (b'/', Some(b'*')) => {
                        state = SqlLexState::BlockComment(comment_depth + 1);
                        index += 1;
                    }
                    (b'*', Some(b'/')) => {
                        state = if comment_depth == 1 {
                            SqlLexState::Code
                        } else {
                            SqlLexState::BlockComment(comment_depth - 1)
                        };
                        index += 1;
                    }
                    _ => {}
                },
                _ => {}
            }
            index += 1;
            continue;
        }
        match (byte, next) {
            (b'-', Some(b'-')) => {
                state = SqlLexState::LineComment;
                index += 2;
                continue;
            }
            (b'/', Some(b'*')) => {
                state = SqlLexState::BlockComment(1);
                index += 2;
                continue;
            }
            (b'\'', _) => state = SqlLexState::SingleQuote,
            (b'"', _) => state = SqlLexState::DoubleQuote,
            (b'[', _) => state = SqlLexState::Bracket,
            (b'`', _) => state = SqlLexState::Backtick,
            (b'(', _) => depth += 1,
            (b')', _) => depth -= 1,
            _ if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                let start = index;
                index += 1;
                while index < end && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                let word = sql[start..index].to_ascii_lowercase();
                if word == "by" {
                    if let Some(clause) = pending_by.take() {
                        active = Some(clause);
                    }
                } else {
                    pending_by = match word.as_str() {
                        "group" => Some(ListClause::GroupBy),
                        "order" => Some(ListClause::OrderBy),
                        _ => None,
                    };
                    match word.as_str() {
                        "select" => active = Some(ListClause::Select),
                        "from" | "where" | "having" | "union" | "except" | "intersect"
                        | "limit" | "offset" | "fetch" => active = None,
                        _ => {}
                    }
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    (state == SqlLexState::Code && depth == 0)
        .then_some(active)
        .flatten()
}

/// Returns `true` when the cursor is inside a SELECT column list — i.e. the
/// last SQL keyword at the same parenthesis depth before the cursor is SELECT,
/// and no clause terminator (FROM, WHERE, …) has been seen since.
///
/// Scans the content up to `cursor` tracking parenthesis depth. At depth 0
/// (relative to the start), records the last SELECT and checks for terminators.
pub(super) fn in_select_column_context(content: &str, cursor: usize) -> bool {
    let bytes = content.as_bytes();
    let end = cursor.min(content.len());
    let mut paren_depth: i32 = 0;
    let mut last_select_at_depth0: Option<usize> = None;

    let mut i = 0;
    while i < end {
        let b = bytes[i];
        if b == b'(' {
            paren_depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            paren_depth -= 1;
            i += 1;
            continue;
        }
        if paren_depth == 0 && (b.is_ascii_alphabetic() || b == b'_') {
            // Extract the word at this position.
            let word_start = i;
            let mut j = i;
            while j < end && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let word = &content[word_start..j];
            let lower = word.to_lowercase();
            if lower == "select" {
                last_select_at_depth0 = Some(word_start);
            } else if SELECT_TERMINATORS.contains(&lower.as_str()) {
                last_select_at_depth0 = None;
            }
            i = j;
            continue;
        }
        i += 1;
    }

    last_select_at_depth0.is_some()
}

/// Returns the net open-paren depth across the full content prefix up to
/// `cursor`. A positive result means there are unclosed `(` before the cursor
/// (e.g. inside a multi-line `COALESCE(`). A zero or negative result means
/// all parens before the cursor are balanced (or over-closed).
pub(super) fn unclosed_parens_before_cursor(content: &str, cursor: usize) -> i32 {
    let end = cursor.min(content.len());
    let mut depth: i32 = 0;
    for ch in content[..end].chars() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
        }
    }
    depth
}

/// Remove one indent unit from the end of an indentation string. Falls back to
/// removing a single tab or up to `unit.len()` trailing whitespace characters.
pub(super) fn remove_one_indent_unit(indent: &str, unit: &str) -> String {
    if indent.is_empty() {
        return indent.to_string();
    }
    if let Some(stripped) = indent.strip_suffix(unit) {
        return stripped.to_string();
    }
    if indent.ends_with('\t') && unit != "\t" {
        return indent[..indent.len() - 1].to_string();
    }
    let ws_len = indent
        .chars()
        .rev()
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    let remove = ws_len.min(unit.len());
    indent[..indent.len() - remove].to_string()
}

/// SQL keywords that, when they appear as the last token on a line, trigger an
/// extra indentation level on the next line. These are clause-introducing or
/// block-starting keywords — not operators like AND/OR or type names.
pub(super) const INDENT_KEYWORDS: &[&str] = &[
    "select", "from", "where", "having", "join", "on", "into", "values", "set", "case", "when",
    "then", "else", "begin", "with", "insert", "update", "delete", "merge", "try", "catch",
    "declare", "if", "while", "for", "foreach", "return", "output", "pivot", "unpivot", "using",
];

/// Second words in multi-word keywords whose last token ("by") alone is not a
/// trigger. Maps the penultimate word to the full phrase (e.g. "group" → "by").
pub(super) const INDENT_BY_PRECURSORS: &[&str] = &["group", "order", "partition"];

/// Returns `true` when `text` (the portion of the current line after leading
/// whitespace, up to the cursor) ends with an indent-triggering SQL keyword.
///
/// Handles single-word keywords (`SELECT`, `FROM`, `WHERE`, …) and the
/// multi-word phrases `GROUP BY`, `ORDER BY`, and `PARTITION BY`. Trailing
/// whitespace is ignored. Comparison is case-insensitive.
pub(super) fn ends_with_indent_keyword(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }

    let last = words.last().unwrap().to_lowercase();

    if last == "by" && words.len() >= 2 {
        let penultimate = words[words.len() - 2].to_lowercase();
        return INDENT_BY_PRECURSORS.contains(&penultimate.as_str());
    }

    INDENT_KEYWORDS.contains(&last.as_str())
}
