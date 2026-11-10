use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_i18n::{
    Language, clear_overrides, compiled_value, set_language, set_overrides_json, text,
};
use std::hint::black_box;

const FIRST_KEY: &str = "settings.appearance.language.applied";
const MIDDLE_KEY: &str = "settings.appearance.language.custom_file_title";
const LAST_KEY: &str = "settings.appearance.language.title";
const SHORT_KEY: &str = "settings.appearance.language.apply_button";
const LONG_KEY: &str = "settings.appearance.language.custom_file_description";
const MISSING_KEY: &str = "settings.appearance.language.missing";

const CATALOG_KEYS: [&str; 12] = [
    "settings.appearance.language.applied",
    "settings.appearance.language.apply_button",
    "settings.appearance.language.browse_button",
    "settings.appearance.language.browser_file_hint",
    "settings.appearance.language.clear_button",
    "settings.appearance.language.custom_file_description",
    "settings.appearance.language.custom_file_title",
    "settings.appearance.language.file_label",
    "settings.appearance.language.intro",
    "settings.appearance.language.language_label",
    "settings.appearance.language.no_file",
    "settings.appearance.language.title",
];

fn full_override_json() -> String {
    // Every real key, including all hot-path grid keys, is an override hit.
    // Parsing and fixture construction remain outside the timed lookup loop.
    include_str!("../translations/es.json").to_owned()
}

fn bench_compiled_value(c: &mut Criterion) {
    for (name, key) in [
        ("compiled_value_first", FIRST_KEY),
        ("compiled_value_middle", MIDDLE_KEY),
        ("compiled_value_last", LAST_KEY),
        ("compiled_value_missing", MISSING_KEY),
    ] {
        c.bench_function(name, |b| {
            b.iter(|| black_box(compiled_value(black_box(Language::English), black_box(key))))
        });
    }
}

fn bench_text(c: &mut Criterion) {
    clear_overrides();
    for (name, language, key) in [
        ("text_english", Language::English, MIDDLE_KEY),
        ("text_spanish", Language::Spanish, MIDDLE_KEY),
        ("text_unknown", Language::English, MISSING_KEY),
        ("text_short", Language::English, SHORT_KEY),
        ("text_long", Language::English, LONG_KEY),
    ] {
        set_language(language);
        c.bench_function(name, |b| b.iter(|| black_box(text(black_box(key)))));
    }
}

fn bench_overrides(c: &mut Criterion) {
    let overrides = full_override_json();
    set_language(Language::English);
    set_overrides_json(&overrides).expect("valid full-catalog overrides");
    for (name, key) in [
        ("override_hit_full_catalog", MIDDLE_KEY),
        ("override_miss_full_catalog", MISSING_KEY),
    ] {
        c.bench_function(name, |b| b.iter(|| black_box(text(black_box(key)))));
    }
}

/// Format compiled catalog messages. Overrides must keep English's placeholder
/// set, so each case uses a real key whose message has that many arguments.
fn bench_format(c: &mut Criterion, name: &str, key: &str, arguments: &[(&str, &str)]) {
    clear_overrides();
    set_language(Language::English);
    c.bench_function(name, |b| {
        b.iter(|| {
            black_box(
                sqlly_i18n::format(black_box(key), black_box(arguments))
                    .expect("valid format template and arguments"),
            )
        })
    });
}

fn bench_formats(c: &mut Criterion) {
    bench_format(
        c,
        "format_1_named_placeholder",
        "datatable.chart.axis.thousands",
        &[("value", "64")],
    );
    bench_format(
        c,
        "format_4_named_placeholders",
        "ai.panel.join_step_declared",
        &[
            ("from_object", "orders"),
            ("from_columns", "customer_id"),
            ("to_object", "customers"),
            ("to_columns", "id"),
        ],
    );
    bench_format(
        c,
        "format_8_named_placeholders",
        "query.execution_plan.node_properties",
        &[
            ("node_id", "7"),
            ("physical_operation", "Hash Match"),
            ("logical_operation", "Inner Join"),
            ("estimated_cost", "0.42"),
            ("cost_percent", "18"),
            ("estimated_rows", "1200"),
            ("actual_rows", "1180"),
            ("warnings", "none"),
        ],
    );
    bench_format(
        c,
        "format_escaped_braces",
        "results.table_export.filename_pattern_hint",
        &[("default_pattern", "{table}_{date}")],
    );
}

fn bench_render_frame(c: &mut Criterion) {
    clear_overrides();
    set_language(Language::English);
    let frame_keys: Vec<&str> = (0..64)
        .map(|index| CATALOG_KEYS[index % CATALOG_KEYS.len()])
        .collect();
    c.bench_function("render_frame_64_labels", |b| {
        b.iter(|| {
            let mut rendered_len = 0usize;
            for key in &frame_keys {
                rendered_len = rendered_len.wrapping_add(black_box(text(black_box(*key))).len());
            }
            black_box(rendered_len)
        })
    });
}

/// Keys the results grid resolves while painting cells: boolean and date
/// display values, pivot blanks/totals, and a relative-time message with an
/// argument. Mirrors the per-cell traffic through the datatable translator.
const GRID_CELL_KEYS: [&str; 8] = [
    "datatable.format.boolean.true",
    "datatable.format.boolean.false",
    "datatable.format.date.day_friday",
    "datatable.format.date.day_friday_abbreviated",
    "datatable.pivot.group.blank",
    "datatable.pivot.totals.total",
    "datatable.format.relative.now",
    "datatable.format.relative.past",
];
const GRID_CELL_LOOKUPS: usize = 10_000;

/// The closure shape `sqlly_datatable::localization::install_translator` takes.
type Translator = fn(&str, &[(&str, &str)]) -> Option<String>;

fn grid_arguments(key: &str) -> &'static [(&'static str, &'static str)] {
    if key == "datatable.format.relative.past" {
        &[("duration", "3 days")]
    } else {
        &[]
    }
}

/// The translator closure as `localization.rs` installed it before
/// `try_text`: a separate English key check, then `text`.
fn legacy_translator(key: &str, arguments: &[(&str, &str)]) -> Option<String> {
    if !sqlly_i18n::contains_key(key) {
        return None;
    }
    if arguments.is_empty() {
        Some(text(key))
    } else {
        sqlly_i18n::format(key, arguments).ok()
    }
}

/// The translator `localization.rs` installed before `try_text_cow`: one
/// catalog lookup through `try_text`, then a separate key check to format.
fn fast_translator(key: &str, arguments: &[(&str, &str)]) -> Option<String> {
    if arguments.is_empty() {
        sqlly_i18n::try_text(key)
    } else if sqlly_i18n::contains_key(key) {
        sqlly_i18n::format(key, arguments).ok()
    } else {
        None
    }
}

/// The translator `localization.rs` installs now: compiled values are
/// borrowed (the one copy is the `String` the datatable trait requires), and
/// formatted messages check the key and format in a single lookup.
fn cow_translator(key: &str, arguments: &[(&str, &str)]) -> Option<String> {
    if arguments.is_empty() {
        sqlly_i18n::try_text_cow(key).map(std::borrow::Cow::into_owned)
    } else {
        sqlly_i18n::format_if_known(key, arguments).and_then(Result::ok)
    }
}

fn run_grid_cells(translator: Translator) -> usize {
    let mut rendered_len = 0usize;
    for index in 0..GRID_CELL_LOOKUPS {
        let key = GRID_CELL_KEYS[index % GRID_CELL_KEYS.len()];
        let value = translator(black_box(key), black_box(grid_arguments(key)));
        rendered_len = rendered_len.wrapping_add(value.map_or(0, |value| value.len()));
    }
    rendered_len
}

fn bench_grid_translator(c: &mut Criterion) {
    let translators: [(&str, Translator); 3] = [
        ("legacy", legacy_translator),
        ("fast", fast_translator),
        ("cow", cow_translator),
    ];
    for (scenario, language, overrides) in [
        ("english", Language::English, None),
        ("spanish", Language::Spanish, None),
        (
            "english_with_overrides",
            Language::English,
            Some(full_override_json()),
        ),
    ] {
        set_language(language);
        clear_overrides();
        if let Some(overrides) = &overrides {
            set_overrides_json(overrides).expect("valid full-catalog overrides");
            for key in GRID_CELL_KEYS {
                assert_eq!(text(key), compiled_value(Language::Spanish, key).unwrap());
            }
        }
        for (name, translator) in translators {
            c.bench_function(
                &format!("grid_translator_10k_cells_{scenario}_{name}"),
                |b| b.iter(|| black_box(run_grid_cells(translator))),
            );
        }
    }
    clear_overrides();
    set_language(Language::English);
}

criterion_group!(
    benches,
    bench_grid_translator,
    bench_compiled_value,
    bench_text,
    bench_overrides,
    bench_formats,
    bench_render_frame
);
criterion_main!(benches);
