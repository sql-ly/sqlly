use std::collections::BTreeSet;

use super::*;

/// Two result sets, the first with a name that needs quoting or cannot be
/// written at all, depending on the family.
fn columns() -> Vec<Vec<String>> {
    let set = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
    vec![
        set(&["Region", "Total", "Order Total", "a;b"]),
        set(&["City", "Latitude", "Longitude"]),
    ]
}

/// The menu with the caret at the end of `text`.
fn offered(text: &str) -> Vec<CompletionItem> {
    complete(text, text.len(), &columns()).map_or_else(Vec::new, |menu| menu.items)
}

fn labels(text: &str) -> Vec<String> {
    offered(text).into_iter().map(|item| item.label).collect()
}

fn inserts(text: &str) -> Vec<String> {
    offered(text)
        .into_iter()
        .map(|item| item.insert_text)
        .collect()
}

fn accept(content: &str, item: &CompletionItem) -> (String, usize) {
    let (start, end) = item.replace_span.expect("directive items carry a span");
    let mut out = String::with_capacity(content.len() + item.insert_text.len());
    out.push_str(&content[..start]);
    out.push_str(&item.insert_text);
    let cursor = out.len();
    out.push_str(&content[end..]);
    (out, cursor)
}

/// Every problem the real parsers report for `sql`.
fn problems(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = crate::sqlly_directive::directive_diagnostics(sql)
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect();
    out.extend(
        crate::result_pivot::parse_view_hints(sql)
            .diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message),
    );
    out
}

/// Walks the menu the way a user accepts through it and holds every line it
/// can produce to the real parsers.
struct Walk {
    /// Lines above the directive.
    head: &'static str,
    /// The directive line up to the caret, and the rest of it.
    before: &'static str,
    after: &'static str,
    /// First-level items to walk; empty walks them all.
    only: &'static [&'static str],
}

impl Walk {
    /// Returns the first-level labels and every finished line.
    fn run(&self) -> (Vec<String>, Vec<String>) {
        let content = format!("{}{}{}\nSELECT 1;", self.head, self.before, self.after);
        let cursor = self.head.len() + self.before.len();
        let menu = complete(&content, cursor, &columns()).expect("the caret is in a directive");
        assert!(
            !menu.items.is_empty(),
            "nothing offered at {:?}",
            self.before
        );
        let mut finished = Vec::new();
        let mut first = Vec::new();
        for item in &menu.items {
            first.push(item.label.clone());
            if self.only.is_empty() || self.only.contains(&item.label.as_str()) {
                self.step(&content, item, 0, &mut finished);
            }
        }
        (first, finished)
    }

    fn step(&self, content: &str, item: &CompletionItem, depth: usize, finished: &mut Vec<String>) {
        let (next, cursor) = accept(content, item);
        // The editor opens the next menu after an item that ends open
        // (`sort=`, `value:Total:`, `filter=Total `).
        let open_ended = item
            .insert_text
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric());
        let following = if open_ended && depth < 6 {
            complete(&next, cursor, &columns()).map_or_else(Vec::new, |menu| menu.items)
        } else {
            Vec::new()
        };
        if !following.is_empty() {
            for item in &following {
                self.step(&next, item, depth + 1, finished);
            }
            return;
        }
        // Free text the user supplies: a number passes every free-form
        // setting the menu leads to.
        let typed = &next[..cursor];
        let filler = if typed.ends_with("between ") {
            "1 and 2"
        } else if typed.ends_with([' ', '=', ':']) {
            "1"
        } else {
            ""
        };
        let line = format!("{}{filler}{}", typed, &next[cursor..]);
        let found = problems(&line);
        assert!(found.is_empty(), "the menu led to {line:?}: {found:?}");
        finished.push(
            line.strip_suffix("\nSELECT 1;")
                .unwrap_or(&line)
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .to_string(),
        );
    }
}

fn walk(before: &'static str, after: &'static str, only: &'static [&'static str]) -> Vec<String> {
    Walk {
        head: "",
        before,
        after,
        only,
    }
    .run()
    .1
}

fn first_level(before: &'static str) -> Vec<String> {
    labels(before)
}

fn assert_has(lines: &[String], expected: &[&str]) {
    for line in expected {
        assert!(
            lines.iter().any(|found| found == line),
            "{line:?} is not reachable; reachable: {lines:#?}"
        );
    }
}

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| value.to_string()).collect()
}

// ── Where the menu appears ─────────────────────────────────────────────

#[test]
fn sql_and_ordinary_comments_are_left_to_the_sql_engine() {
    for text in [
        "SELECT ",
        "SELECT 1 -",
        "-- a note",
        "-- s",
        "-- sqllyish",
        "-- squid",
        "--- sqlly ",
        "SELECT '-- sqlly ",
    ] {
        assert!(
            complete(text, text.len(), &columns()).is_none(),
            "{text:?} must fall through"
        );
    }
    // The caret before the dashes is still SQL.
    assert!(complete("  -- sqlly view:", 1, &columns()).is_none());
}

#[test]
fn free_text_inside_a_directive_offers_nothing_but_keeps_sql_completion_out() {
    for text in [
        "-- sqlly name 1: Ord",
        "-- sqlly set region = We",
        "-- sqlly column: Net",
        "-- sqlly format: column=Total; template=ab",
        "-- sqlly unknown: x",
    ] {
        let menu = complete(text, text.len(), &columns())
            .unwrap_or_else(|| panic!("{text:?} is a directive"));
        assert!(menu.items.is_empty(), "{text:?} → {:?}", menu.items);
    }
}

#[test]
fn bare_dashes_offer_a_valid_starter_for_every_family() {
    for trigger in ["--", "-- ", "  --\t"] {
        let menu = complete(trigger, trigger.len(), &columns()).expect("starters");
        assert_eq!(menu.replace_start, trigger.find("--").unwrap());
        assert_eq!(menu.items.len(), STARTERS.len(), "{trigger:?}");
        for item in &menu.items {
            assert_eq!(item.kind, CompletionKind::Snippet);
            let (sql, _) = accept(trigger, item);
            let sql = format!("{}\nSELECT 1;", sql.trim_start());
            let found = problems(&sql);
            assert!(
                found.is_empty(),
                "starter {:?}: {found:?}",
                item.insert_text
            );
        }
    }
    assert_eq!(
        inserts("-- ")[0],
        "-- sqlly view:pivot\n-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum"
    );
}

#[test]
fn starters_below_the_first_line_leave_out_what_only_the_first_line_honors() {
    let text = "SELECT 1;\n-- ";
    let menu = complete(text, text.len(), &columns()).expect("starters");
    assert!(menu.items.len() < STARTERS.len());
    for item in &menu.items {
        assert!(
            !item.insert_text.contains("view:") && !item.insert_text.contains("2x2"),
            "{:?}",
            item.insert_text
        );
        let (sql, _) = accept(text, item);
        let found = problems(&format!("{sql}\nSELECT 2;"));
        assert!(
            found.is_empty(),
            "starter {:?}: {found:?}",
            item.insert_text
        );
    }
}

#[test]
fn starters_stay_out_of_trailing_comments_and_comments_with_text() {
    assert!(complete("SELECT 1 --", 11, &columns()).is_none());
    assert!(complete("-- note", 2, &columns()).is_none());
}

#[test]
fn a_started_namespace_completes_to_the_namespace() {
    for text in ["-- sq", "--SQL", "-- sqll"] {
        let items = offered(text);
        assert_eq!(items.len(), 1, "{text:?}");
        assert_eq!(accept(text, &items[0]).0, "-- sqlly ");
    }
}

#[test]
fn the_namespace_offers_every_family_in_every_spelling() {
    let expected: Vec<&str> = FAMILIES.iter().map(|family| family.label).collect();
    for text in ["-- sqlly ", "--sqlly", "--\tsqlly ", "-- SQLLY "] {
        assert_eq!(labels(text), expected, "{text:?}");
        // Accepting rewrites the comment in the canonical spelling.
        for item in offered(text) {
            assert!(item.insert_text.starts_with("-- sqlly "), "{item:?}");
            assert_eq!(item.replace_span, Some((0, text.len())));
        }
    }
    // Every family the lexer documents has a keyword in the menu.
    for doc in crate::sqlly_directive::DIRECTIVES {
        let keyword = doc
            .syntax
            .trim_start_matches("-- sqlly ")
            .split([':', ' ', '<'])
            .next()
            .unwrap_or_default();
        assert!(
            keyword.is_empty() || expected.iter().any(|label| label.starts_with(keyword)),
            "no menu entry for {:?}",
            doc.syntax
        );
    }
}

#[test]
fn every_family_keyword_classifies_as_its_directive() {
    for item in offered("-- sqlly ") {
        let line = match item.label.as_str() {
            "set" => format!("{}a = 1", item.insert_text),
            "param:" => format!("{}@a int", item.insert_text),
            _ => format!("{}x", item.insert_text),
        };
        let line = if item.label.contains('x') {
            item.insert_text.clone()
        } else {
            line
        };
        assert!(classify(&line).is_some(), "{line:?} is not a directive");
    }
}

#[test]
fn the_family_menu_filters_by_the_typed_keyword() {
    assert_eq!(labels("-- sqlly gr"), ["grid:", "grid <n>:"]);
    assert_eq!(labels("-- sqlly C"), ["chart:", "conditional:", "column:"]);
    assert_eq!(
        inserts("--sqlly fo"),
        ["-- sqlly format: ", "-- sqlly focus: "]
    );
    assert_eq!(labels("-- sqlly 2"), ["2x1", "2x2", "2x3"]);
    assert!(labels("-- sqlly zz").is_empty());
}

#[test]
fn view_and_canvas_are_offered_only_on_the_first_line() {
    let below = labels("SELECT 1;\n-- sqlly ");
    assert!(!below.iter().any(|label| label == "view:" || label == "2x2"));
    assert!(below.iter().any(|label| label == "grid:"));
    // A lock block's opener does not count as a line.
    let content = "/* sqlly lock\n-- sqlly \n*/\nSELECT 1;";
    let cursor = "/* sqlly lock\n-- sqlly ".len();
    let locked = complete(content, cursor, &columns()).expect("families");
    assert!(locked.items.iter().any(|item| item.label == "view:"));
}

#[test]
fn a_trailing_comment_offers_only_the_families_honored_there() {
    assert_eq!(labels("SELECT 1 -- sqlly "), ["name <n>:", "date:"]);
}

#[test]
fn completing_the_keyword_of_a_written_directive_replaces_only_the_keyword() {
    let content = "-- sqlly gr: sort=Total";
    let menu = complete(content, "-- sqlly gr".len(), &columns()).expect("families");
    assert_eq!(
        accept(content, &menu.items[0]).0,
        "-- sqlly grid: sort=Total"
    );
    // Mid-word: the rest of the word goes too.
    let content = "-- sqlly chrt: kind:bar";
    let menu = complete(content, "-- sqlly ch".len(), &columns()).expect("families");
    assert_eq!(
        accept(content, &menu.items[0]).0,
        "-- sqlly chart: kind:bar"
    );
}

// ── Every setting and value, against the real parsers ──────────────────

#[test]
fn view_layout_lines_and_focus_offer_every_value() {
    assert_eq!(labels("-- sqlly view:"), ["grid", "pivot", "chart", "map"]);
    assert_eq!(labels("-- sqlly view: p"), ["pivot"]);
    assert_eq!(labels("-- sqlly layout: "), ["stacked", "tabbed", "canvas"]);
    assert_eq!(labels("-- sqlly focus: "), ["1", "2"]);
    assert_eq!(labels("-- sqlly lines 2: "), ["5", "10", "15", "20", "30"]);
    for view in labels("-- sqlly view:") {
        let found = crate::sqlly_directive::directive_diagnostics(&format!(
            "-- sqlly view:{view}\nSELECT 1;"
        ));
        assert!(found.is_empty(), "view:{view}");
    }
    for before in [
        "-- sqlly layout: ",
        "-- sqlly focus: ",
        "-- sqlly lines: ",
        "-- sqlly lines 2: ",
    ] {
        assert!(!walk(before, "", &[]).is_empty());
    }
    // A value already written in full is not suggested again.
    assert!(labels("-- sqlly view:pivot").is_empty());
    assert!(labels("-- sqlly lines: 12").is_empty());
}

#[test]
fn the_result_number_follows_an_indexed_family() {
    assert_eq!(inserts("-- sqlly grid "), ["1: ", "2: "]);
    assert_eq!(inserts("-- sqlly map 2"), ["2: "]);
    assert_eq!(inserts("-- sqlly name "), ["1: ", "2: "]);
    let detail = &offered("-- sqlly lines ")[1].detail;
    assert!(detail.contains("City"), "{detail}");
}

#[test]
fn format_offers_the_column_then_each_rule_and_option() {
    // `column=` leads; the parser reads it from the first setting only.
    assert_eq!(first_level("-- sqlly format: "), ["column="]);
    // A name holding `;` cannot be written in a format rule.
    assert_eq!(
        labels("-- sqlly format: column="),
        [
            "Region",
            "Total",
            "Order Total",
            "City",
            "Latitude",
            "Longitude"
        ]
    );
    assert_has(
        &walk("-- sqlly format: ", "; number=2", &[]),
        &["-- sqlly format: column=Order Total; number=2"],
    );

    let rules = first_level("-- sqlly format: column=Total; ");
    assert_eq!(
        set(&rules.iter().map(String::as_str).collect::<Vec<_>>()),
        set(&[
            "number=",
            "date=",
            "boolean=",
            "binary=",
            "text=",
            "json=",
            "template=",
            "mask=",
            "epoch=",
            "width=",
            "zone=",
        ])
    );
    let lines = walk(
        "-- sqlly format: column=Total; ",
        "",
        &[
            "number=",
            "date=",
            "boolean=",
            "binary=",
            "text=",
            "json=",
            "template=",
            "mask=",
            "epoch=",
        ],
    );
    assert_has(
        &lines,
        &[
            "-- sqlly format: column=Total; number=2",
            "-- sqlly format: column=Total; date=%Y-%m-%d %H:%M:%S",
            "-- sqlly format: column=Total; boolean=Yes/No",
            "-- sqlly format: column=Total; binary=hex",
            "-- sqlly format: column=Total; binary=base64",
            "-- sqlly format: column=Total; binary=uuid",
            "-- sqlly format: column=Total; binary=ulid",
            "-- sqlly format: column=Total; text=upper",
            "-- sqlly format: column=Total; text=lower",
            "-- sqlly format: column=Total; text=title",
            "-- sqlly format: column=Total; text=none",
            "-- sqlly format: column=Total; mask=0/4",
            "-- sqlly format: column=Total; epoch=auto",
            "-- sqlly format: column=Total; epoch=seconds",
            "-- sqlly format: column=Total; epoch=milliseconds",
            "-- sqlly format: column=Total; epoch=s",
            "-- sqlly format: column=Total; epoch=ms",
        ],
    );

    // With a rule on the line the other rules are gone; a number gains
    // its two options.
    assert_eq!(
        first_level("-- sqlly format: column=Total; number=2; "),
        ["thousands=", "style=", "width=", "zone="]
    );
    assert_has(
        &walk("-- sqlly format: column=Total; number=2; ", "", &[]),
        &[
            "-- sqlly format: column=Total; number=2; thousands=true",
            "-- sqlly format: column=Total; number=2; thousands=false",
            "-- sqlly format: column=Total; number=2; style=fixed",
            "-- sqlly format: column=Total; number=2; style=percent",
            "-- sqlly format: column=Total; number=2; style=bytes",
            "-- sqlly format: column=Total; number=2; width=40dp",
        ],
    );
    assert_has(
        &walk("-- sqlly format: column=Total; date=%Y; ", "", &[]),
        &[
            "-- sqlly format: column=Total; date=%Y; zone=utc",
            "-- sqlly format: column=Total; date=%Y; zone=local",
            "-- sqlly format: column=Total; date=%Y; zone=stored",
        ],
    );
    // Settings already on the line, either side of the caret, are not
    // offered again.
    let content = "-- sqlly format: column=Total; ; width=40dp";
    let menu = complete(content, "-- sqlly format: column=Total; ".len(), &columns()).unwrap();
    assert!(!menu.items.iter().any(|item| item.label == "width="));
}

#[test]
fn a_time_zone_is_found_by_any_part_of_its_name() {
    let before = "-- sqlly format: column=At; date=%Y; zone=";
    assert_eq!(labels(before), ["utc", "local", "stored"]);
    let typed = format!("{before}york");
    let items = offered(&typed);
    assert_eq!(items[0].label, "America/New_York");
    let (line, _) = accept(&typed, &items[0]);
    assert_eq!(line, format!("{before}America/New_York"));
    assert!(problems(&format!("{line}\nSELECT 1;")).is_empty());
    assert!(labels(&format!("{before}Europe/")).len() > 10);
}

#[test]
fn pivot_offers_every_setting_aggregate_and_format_option() {
    assert_eq!(
        first_level("-- sqlly pivot: "),
        [
            "row:",
            "column:",
            "value:",
            "format:",
            "valueformat:",
            "result:"
        ]
    );
    // Names are written bare, so one holding a separator is left out.
    assert_eq!(
        labels("-- sqlly pivot: result:1; row:"),
        ["Region", "Total", "Order Total"]
    );
    assert_eq!(
        labels("-- sqlly pivot: result:2; row:"),
        ["City", "Latitude", "Longitude"]
    );
    // A list leaves out the names it already holds.
    assert_eq!(
        labels("-- sqlly pivot: result:1; row:Region, "),
        ["Total", "Order Total"]
    );

    assert_has(
        &walk(
            "-- sqlly pivot: ",
            "; value:Total:sum",
            &["row:", "column:"],
        ),
        &[
            "-- sqlly pivot: row:Region; value:Total:sum",
            "-- sqlly pivot: column:Order Total; value:Total:sum",
        ],
    );
    assert_has(
        &walk("-- sqlly pivot: row:Region; ", "", &["value:"]),
        &[
            "-- sqlly pivot: row:Region; value:Total:sum",
            "-- sqlly pivot: row:Region; value:Total:count",
            "-- sqlly pivot: row:Region; value:Total:avg",
            "-- sqlly pivot: row:Region; value:Total:min",
            "-- sqlly pivot: row:Region; value:Total:max",
            "-- sqlly pivot: row:Region; value:Total:first",
        ],
    );
    let before = "-- sqlly pivot: row:Region; value:Total:sum; ";
    assert_eq!(
        first_level(before),
        ["column:", "format:", "valueformat:", "result:"]
    );
    assert_has(
        &walk(before, "", &["format:", "valueformat:", "result:"]),
        &[
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:align=left",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:align=center",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:align=right",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:decimals=2",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:thousands=true",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:red=true",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:parens=false",
            "-- sqlly pivot: row:Region; value:Total:sum; format:Total:default",
            "-- sqlly pivot: row:Region; value:Total:sum; valueformat:align=right",
            "-- sqlly pivot: row:Region; value:Total:sum; valueformat:default",
            "-- sqlly pivot: row:Region; value:Total:sum; result:2",
        ],
    );
    // Options chain with commas; a used one is gone, and `default` only
    // stands alone.
    assert_eq!(
        labels("-- sqlly pivot: valueformat:align=right,"),
        ["decimals=", "thousands=", "red=", "parens="]
    );
    // `format:` repeats, one per field.
    assert!(
        labels("-- sqlly pivot: format:Total:default; ")
            .iter()
            .any(|label| label == "format:")
    );
}

#[test]
fn chart_offers_every_setting_and_value() {
    assert_eq!(
        first_level("-- sqlly chart: "),
        [
            "kind:",
            "label:",
            "values:",
            "aggregate:",
            "order:",
            "top:",
            "navigate:",
            "result:"
        ]
    );
    assert_has(
        &walk("-- sqlly chart: ", "; values:Total", &["kind:"]),
        &[
            "-- sqlly chart: kind:bar; values:Total",
            "-- sqlly chart: kind:line; values:Total",
            "-- sqlly chart: kind:area; values:Total",
            "-- sqlly chart: kind:scatter; values:Total",
            "-- sqlly chart: kind:histogram; values:Total",
            "-- sqlly chart: kind:pie; values:Total",
            "-- sqlly chart: kind:donut; values:Total",
        ],
    );
    let before = "-- sqlly chart: kind:bar; values:Total; ";
    assert_eq!(
        first_level(before),
        [
            "label:",
            "aggregate:",
            "order:",
            "top:",
            "navigate:",
            "result:"
        ]
    );
    assert_has(
        &walk(before, "", &[]),
        &[
            "-- sqlly chart: kind:bar; values:Total; label:Region",
            "-- sqlly chart: kind:bar; values:Total; aggregate:none",
            "-- sqlly chart: kind:bar; values:Total; aggregate:sum",
            "-- sqlly chart: kind:bar; values:Total; aggregate:average",
            "-- sqlly chart: kind:bar; values:Total; aggregate:minimum",
            "-- sqlly chart: kind:bar; values:Total; aggregate:maximum",
            "-- sqlly chart: kind:bar; values:Total; aggregate:count",
            "-- sqlly chart: kind:bar; values:Total; order:source",
            "-- sqlly chart: kind:bar; values:Total; order:category",
            "-- sqlly chart: kind:bar; values:Total; order:value",
            "-- sqlly chart: kind:bar; values:Total; top:10",
            "-- sqlly chart: kind:bar; values:Total; navigate:true",
            "-- sqlly chart: kind:bar; values:Total; navigate:false",
            "-- sqlly chart: kind:bar; values:Total; result:1",
        ],
    );
    assert_has(
        &walk("-- sqlly chart: kind:bar; ", "", &["values:"]),
        &["-- sqlly chart: kind:bar; values:Order Total"],
    );
    assert_eq!(
        labels("-- sqlly chart: kind:bar; result:1; values:Total,"),
        ["Region", "Order Total"]
    );
    // `value` is the same setting as `values`.
    assert!(
        !labels("-- sqlly chart: value:Total; ")
            .iter()
            .any(|label| label == "values:")
    );
}

#[test]
fn conditional_offers_a_test_with_its_styles_or_a_whole_column_rule() {
    assert_eq!(
        first_level("-- sqlly conditional: "),
        [
            "column=", "when=", "text=", "fill=", "bold", "scale=", "bar="
        ]
    );
    assert_has(
        &walk("-- sqlly conditional: ", "; when=null; bold", &["column="]),
        &["-- sqlly conditional: column=Order Total; when=null; bold"],
    );
    assert_has(
        &walk(
            "-- sqlly conditional: column=Total; ",
            "; fill=#F8696B",
            &["when="],
        ),
        &[
            "-- sqlly conditional: column=Total; when=null; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=not null; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=> 1; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=< 1; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=between 0 and 100; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=is 1; fill=#F8696B",
            "-- sqlly conditional: column=Total; when=contains 1; fill=#F8696B",
        ],
    );
    // A test takes styles, never a scale or bar.
    let before = "-- sqlly conditional: column=Total; when=null; ";
    assert_eq!(first_level(before), ["text=", "fill=", "bold"]);
    assert_has(
        &walk(before, "", &[]),
        &[
            "-- sqlly conditional: column=Total; when=null; text=#B00020",
            "-- sqlly conditional: column=Total; when=null; fill=#63BE7B",
            "-- sqlly conditional: column=Total; when=null; bold",
        ],
    );
    // A whole-column rule stands alone.
    assert_has(
        &walk(
            "-- sqlly conditional: column=Total; ",
            "",
            &["scale=", "bar="],
        ),
        &[
            "-- sqlly conditional: column=Total; scale=#63BE7B..#F8696B",
            "-- sqlly conditional: column=Total; bar=#5B9BD5",
        ],
    );
    assert!(first_level("-- sqlly conditional: column=Total; bar=#5B9BD5; ").is_empty());
}

#[test]
fn grid_offers_every_setting_with_quoted_names_and_every_filter_operator() {
    assert_eq!(
        first_level("-- sqlly grid: "),
        [
            "order=", "hide=", "freeze=", "sort=", "group=", "filter=", "label="
        ]
    );
    let lines = walk("-- sqlly grid: ", "", &[]);
    let mut expected = vec![
        "-- sqlly grid: order=\"Order Total\"".to_string(),
        "-- sqlly grid: hide=\"a;b\"".to_string(),
        "-- sqlly grid: freeze=1".to_string(),
        "-- sqlly grid: sort=Total".to_string(),
        "-- sqlly grid: group=Region".to_string(),
        "-- sqlly grid: label=Total:off".to_string(),
        "-- sqlly grid: label=Total:\"Order Total\"".to_string(),
    ];
    for (operator, _) in FILTER_OPERATORS {
        let value = if operator.ends_with("between") {
            "1 and 2"
        } else {
            "1"
        };
        expected.push(format!("-- sqlly grid: filter=Total {operator} {value}"));
        expected.push(format!(
            "-- sqlly grid: filter=\"Order Total\" {operator} {value}"
        ));
    }
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_has(&lines, &expected);

    // A sort key takes a direction, and a list continues after a comma.
    assert_eq!(labels("-- sqlly grid: sort=Total "), ["asc", "desc"]);
    assert_eq!(labels("-- sqlly grid: sort=\"Order Total\" d"), ["desc"]);
    assert_eq!(labels("-- sqlly grid 1: sort=Total desc, Re"), ["Region"]);
    for line in [
        "-- sqlly grid: sort=Total asc",
        "-- sqlly grid: sort=\"Order Total\" desc, Region asc",
    ] {
        assert!(problems(&format!("{line}\nSELECT 1;")).is_empty(), "{line}");
    }
    // The index scopes the columns to its result set.
    assert_eq!(
        labels("-- sqlly grid 2: group="),
        ["City", "Latitude", "Longitude"]
    );
    assert!(!walk("-- sqlly grid 2: ", "", &["hide="]).is_empty());
    // A `;` inside a quoted name does not end the setting.
    assert_eq!(labels("-- sqlly grid: hide=\"a;b\", Reg"), ["Region"]);
    // One-off settings are not offered twice; filters and labels repeat.
    assert_eq!(
        first_level("-- sqlly grid: sort=Total; filter=Total > 1; "),
        ["order=", "hide=", "freeze=", "group=", "filter=", "label="]
    );
    // A name typed with its opening quote completes to the quoted name.
    let typed = "-- sqlly grid: group=\"Ord";
    let items = offered(typed);
    assert_eq!(
        accept(typed, &items[0]).0,
        "-- sqlly grid: group=\"Order Total\""
    );
}

#[test]
fn map_offers_every_key_and_value() {
    let keys = first_level("-- sqlly map: ");
    let expected: Vec<String> = crate::map_view::directive::KEYS
        .iter()
        .map(|key| format!("{key}="))
        .collect();
    assert_eq!(keys, expected);

    let before = "-- sqlly map 2: lat=Latitude; lon=Longitude; ";
    let lines = walk(before, "", &[]);
    let mut reached: BTreeSet<&str> = BTreeSet::new();
    for line in &lines {
        let setting = line.strip_prefix(before).expect("the walked line");
        reached.insert(setting.split('=').next().unwrap_or_default());
    }
    let remaining: BTreeSet<&str> = crate::map_view::directive::KEYS
        .iter()
        .copied()
        .filter(|key| !matches!(*key, "lat" | "lon"))
        .collect();
    assert_eq!(reached, remaining);
    let settings = [
        "label=City",
        "mode=points",
        "mode=heatmap",
        "mode=both",
        "palette=heat",
        "palette=fire",
        "palette=viridis",
        "palette=ocean",
        "palette=accent",
        "cluster=on",
        "cluster=off",
        "basemap=osm",
        "basemap=none",
        "fit=data",
        "click=select",
        "click=none",
        "open=auto",
        "open=manual",
        "srid=4326",
        "zoom=10",
        "center=0,0",
    ];
    let expected: Vec<String> = settings
        .iter()
        .map(|setting| format!("{before}{setting}"))
        .collect();
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_has(&lines, &expected);

    assert_has(
        &walk("-- sqlly map 2: ", "; lon=Longitude", &["lat="]),
        &["-- sqlly map 2: lat=Latitude; lon=Longitude"],
    );
    assert_has(
        &walk("-- sqlly map 2: lat=Latitude; ", "", &["lon="]),
        &["-- sqlly map 2: lat=Latitude; lon=Longitude"],
    );
    assert_has(
        &walk("-- sqlly map 2: ", "", &["geom="]),
        &["-- sqlly map 2: geom=City"],
    );
    assert_eq!(labels("-- sqlly map: tooltip=City, Lat"), ["Latitude"]);
    assert_eq!(labels("-- sqlly map 1: label=\"Or"), ["Order Total"]);
}

#[test]
fn a_computed_column_completes_references_functions_and_options() {
    // Column references, quoted where the name needs it.
    let typed = "-- sqlly column: Net = !";
    assert_eq!(
        inserts(typed)[..4],
        ["Region", "Total", "\"Order Total\"", "\"a;b\""]
    );
    let typed = "-- sqlly column: Net = !Total * !\"Ord";
    let items = offered(typed);
    assert_eq!(
        accept(typed, &items[0]).0,
        "-- sqlly column: Net = !Total * !\"Order Total\""
    );
    // Worksheet functions open their argument list.
    let typed = "-- sqlly column: Net = ROU";
    let items = offered(typed);
    assert!(
        items.iter().any(|item| item.insert_text == "ROUND("),
        "{items:?}"
    );
    assert!(
        items
            .iter()
            .all(|item| item.kind == CompletionKind::Function)
    );
    // `@` reaches the query's set variables.
    let content = "-- sqlly set rate = 2\n-- sqlly column: Net = !Total * @";
    assert_eq!(labels(content), ["rate"]);
    // Nothing is a name inside a text literal or a number.
    assert!(labels("-- sqlly column: Net = \"ab").is_empty());
    assert!(labels("-- sqlly column: Net = 12").is_empty());
    assert!(labels("-- sqlly column: Net = ").is_empty());

    // A `;` inside the formula's parentheses does not start the options.
    assert!(labels("-- sqlly column: Net = IF(!Total > 1; ").is_empty());
    let before = "-- sqlly column: Net = !Total * 2; ";
    assert_eq!(first_level(before), ["after=", "result="]);
    assert_has(
        &walk(before, "", &[]),
        &[
            "-- sqlly column: Net = !Total * 2; after=Order Total",
            "-- sqlly column: Net = !Total * 2; result=1",
        ],
    );
    assert_eq!(
        labels("-- sqlly column: Net = !Total; after=Region; "),
        ["result="]
    );
    // `result=` scopes the references, wherever it sits on the line.
    let content = "-- sqlly column: Net = !; result=2";
    let menu = complete(content, "-- sqlly column: Net = !".len(), &columns()).unwrap();
    let names: Vec<&str> = menu.items.iter().map(|item| item.label.as_str()).collect();
    assert_eq!(names, ["City", "Latitude", "Longitude"]);
}

#[test]
fn a_placement_offers_every_cell_of_the_declared_canvas() {
    let content = "-- sqlly 2x3\n-- sqlly result: ";
    assert_eq!(labels(content), ["1|1", "1|2", "1|3", "2|1", "2|2", "2|3"]);
    assert_eq!(
        labels("-- sqlly 2x3\n-- sqlly result: 2"),
        ["2|1", "2|2", "2|3"]
    );
    // No canvas line yet: the default board.
    assert_eq!(labels("SELECT 1;\n-- sqlly result: ").len(), 4);
    let (_, lines) = Walk {
        head: "-- sqlly 2x2\n",
        before: "-- sqlly result: ",
        after: "",
        only: &[],
    }
    .run();
    assert_eq!(lines.len(), 4);
}

#[test]
fn a_parameter_offers_the_querys_undeclared_names_then_every_type() {
    let query = "SELECT * FROM t WHERE a = @customer_id AND b = @from_date AND c = @@ROWCOUNT AND d = 'x@y.z'";
    let content = format!("-- sqlly param: @from_date date\n-- sqlly param: \n{query}");
    let cursor = content.find("param: \n").unwrap() + "param: ".len();
    let menu = complete(&content, cursor, &columns()).unwrap();
    let names: Vec<&str> = menu.items.iter().map(|item| item.label.as_str()).collect();
    assert_eq!(names, ["@customer_id"]);

    let types = labels("-- sqlly param: @id ");
    assert_eq!(
        types,
        [
            "auto", "text", "int", "decimal", "bool", "date", "datetime", "time", "uuid", "json",
            "binary"
        ]
    );
    assert_eq!(walk("-- sqlly param: @id ", "", &[]).len(), types.len());
    assert_eq!(labels("-- sqlly param: @id da"), ["date", "datetime"]);
    assert_eq!(
        labels("-- sqlly param: @on bool = "),
        ["NULL", "true", "false"]
    );
    assert_eq!(labels("-- sqlly param: @id int = "), ["NULL"]);
    for line in [
        "-- sqlly param: @on bool = true",
        "-- sqlly param: @id int = NULL",
    ] {
        assert!(problems(&format!("{line}\nSELECT 1;")).is_empty(), "{line}");
    }
}

#[test]
fn a_date_marker_offers_the_example_phrases() {
    let phrases = labels("SELECT 1 WHERE d > '2026-01-01' -- sqlly date:");
    assert!(phrases.iter().any(|phrase| phrase == "ytd"), "{phrases:?}");
    assert_eq!(labels("-- sqlly date: tod"), ["today"]);
}

// ── Shape of the items ─────────────────────────────────────────────────

#[test]
fn accepting_inside_a_word_replaces_the_whole_word() {
    let content = "-- sqlly chart: kind:bxr; values:Total";
    let menu = complete(content, "-- sqlly chart: kind:b".len(), &columns()).unwrap();
    assert_eq!(menu.items[0].label, "bar");
    assert_eq!(
        accept(content, &menu.items[0]).0,
        "-- sqlly chart: kind:bar; values:Total"
    );
}

#[test]
fn columns_match_anywhere_in_the_name_with_prefix_matches_first() {
    assert_eq!(
        labels("-- sqlly grid 1: group=tot"),
        ["Total", "Order Total"]
    );
    assert_eq!(
        labels("-- sqlly grid 2: group=itude"),
        ["Latitude", "Longitude"]
    );
}

#[test]
fn every_item_is_anchored_at_the_menus_replace_start_and_ranked_in_order() {
    for text in [
        "--",
        "-- sq",
        "-- sqlly ",
        "-- sqlly grid: ",
        "-- sqlly grid: sort=To",
        "-- sqlly pivot: row:Region; value:Total:",
        "-- sqlly column: Net = !\"Or",
    ] {
        let menu = complete(text, text.len(), &columns()).unwrap();
        assert!(!menu.items.is_empty(), "{text:?}");
        for pair in menu.items.windows(2) {
            assert!(pair[0].score > pair[1].score);
        }
        for item in &menu.items {
            let (start, end) = item.replace_span.unwrap();
            assert_eq!(start, menu.replace_start, "{text:?}");
            assert!(end >= text.len());
        }
    }
}

#[test]
fn without_a_run_there_are_no_columns_but_everything_else_works() {
    let none: &[Vec<String>] = &[];
    let at = |text: &str| -> Vec<String> {
        complete(text, text.len(), none)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.label)
            .collect()
    };
    assert!(at("-- sqlly grid: sort=").is_empty());
    assert_eq!(at("-- sqlly chart: kind:b"), ["bar"]);
    assert_eq!(at("-- sqlly focus: "), ["1", "2", "3"]);
}

#[test]
fn crlf_buffers_and_multibyte_text_complete_like_any_other() {
    let content = "-- sqlly view:\r\nSELECT 1;";
    let menu = complete(content, "-- sqlly view:".len(), &columns()).unwrap();
    assert_eq!(menu.items.len(), 4);
    let content = "SELECT 'é';\r\n-- sqlly grid: group=Ré";
    let menu = complete(content, content.len(), &columns()).unwrap();
    assert!(menu.items.is_empty());
    // A caret inside a multi-byte character is pulled back to a boundary.
    assert!(complete("-- é", 4, &columns()).is_none());
}
