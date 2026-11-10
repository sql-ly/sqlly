use sqlly_core::ResultColumn;
use sqlly_datatable::{
    BooleanFormat, ColumnOverride, DateFormat, GridConfig, NumberFormat, StringFormat, TextCase,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryColumnFormat {
    Number {
        decimals: usize,
        thousands: bool,
    },
    Date {
        format: String,
    },
    Boolean {
        true_text: String,
        false_text: String,
    },
    Binary(BinaryFormat),
    Text(TextCase),
    Mask {
        keep_prefix: usize,
        keep_suffix: usize,
    },
    Epoch {
        unit: crate::preferences::EpochUnit,
    },
    JsonPath {
        path: String,
    },
    Template {
        template: String,
    },
    NumberStyle {
        style: crate::preferences::NumberStyle,
        decimals: u8,
        thousands: bool,
    },
    /// No display rule — the line only carries options (e.g. `width=`).
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormat {
    Hex,
    Base64,
    /// 16-byte values as a canonical hyphenated UUID; other lengths as hex.
    Uuid,
    /// 16-byte values as a 26-character ULID; other lengths as hex.
    Ulid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFormatHint {
    pub column: String,
    pub format: QueryColumnFormat,
    /// `width=<n>dp`: the column width in font-relative units (see
    /// [`dp_to_px`]).
    pub width_dp: Option<u32>,
    /// `zone=<utc|local|stored|IANA name>`: the display time zone for a
    /// `date=` or `epoch=` rule. Wins over the column override, the
    /// connection, and the global Date zone.
    pub zone: Option<DirectiveZone>,
}

/// A `zone=` option: a concrete display time zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveZone {
    pub mode: crate::preferences::DateTimeZoneMode,
    /// The IANA name for `AlwaysInTimeZone`; ignored otherwise.
    pub identifier: String,
}

impl DirectiveZone {
    /// Parse a `zone=` value: `utc`, `local`, `stored` (also `as-stored`),
    /// or an IANA zone name (case-insensitive). `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        use crate::preferences::DateTimeZoneMode;
        let value = value.trim();
        let fixed = |mode| {
            Some(Self {
                mode,
                identifier: String::new(),
            })
        };
        match value.to_ascii_lowercase().as_str() {
            "" => None,
            "utc" | "z" => fixed(DateTimeZoneMode::AlwaysUtc),
            "local" => fixed(DateTimeZoneMode::AlwaysLocal),
            "stored" | "as-stored" | "native" => fixed(DateTimeZoneMode::NativeAsStored),
            _ => {
                let tz = value.parse::<chrono_tz::Tz>().ok().or_else(|| {
                    chrono_tz::TZ_VARIANTS
                        .iter()
                        .copied()
                        .find(|tz| tz.name().eq_ignore_ascii_case(value))
                })?;
                Some(Self {
                    mode: DateTimeZoneMode::AlwaysInTimeZone,
                    identifier: tz.name().to_string(),
                })
            }
        }
    }

    /// The `zone=` value that reproduces this zone.
    pub fn directive_value(&self) -> String {
        use crate::preferences::DateTimeZoneMode;
        match self.mode {
            DateTimeZoneMode::AlwaysUtc => "UTC".into(),
            DateTimeZoneMode::AlwaysLocal => "local".into(),
            DateTimeZoneMode::NativeAsStored => "stored".into(),
            DateTimeZoneMode::AlwaysInTimeZone => self.identifier.clone(),
        }
    }

    /// `fmt` with this zone as its display zone.
    pub fn apply_to_date_format(&self, fmt: &mut crate::preferences::DateFormat) {
        fmt.time_zone_mode = self.mode.clone();
        fmt.time_zone_identifier = self.identifier.clone();
        fmt.zone_pinned = true;
    }
}

/// The `zone=` a query sets for `column`.
pub fn directive_zone_for(column: &str, hints: &[QueryFormatHint]) -> Option<DirectiveZone> {
    hints
        .iter()
        .find(|hint| hint.column.eq_ignore_ascii_case(column))
        .and_then(|hint| hint.zone.clone())
}

/// Font-relative width units per grid font size: 1dp is a quarter of the
/// font size, so 20dp is 50 px at 10 pt and 60 px at 12 pt — a column keeps
/// roughly the same number of visible characters at any font size or zoom.
pub const DP_PER_FONT_SIZE: f32 = 4.0;

/// Widest `width=` accepted, in dp.
pub const MAX_WIDTH_DP: u32 = 4000;

/// Pixels for `dp` at the grid font size `font_size`.
pub fn dp_to_px(dp: u32, font_size: f32) -> f32 {
    dp as f32 * font_size.max(1.0) / DP_PER_FONT_SIZE
}

/// The dp a `px`-wide column measures at `font_size` (rounded, at least 1).
pub fn px_to_dp(px: f32, font_size: f32) -> u32 {
    let dp = (px * DP_PER_FONT_SIZE / font_size.max(1.0)).round();
    (dp.max(1.0) as u32).min(MAX_WIDTH_DP)
}

pub fn parse(sql: &str) -> Vec<QueryFormatHint> {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let mut hints: Vec<QueryFormatHint> = Vec::new();
    for line in sql.lines() {
        // The shared lexer owns the (deliberately strict) `-- sqlly format:`
        // spelling; the comment must start the line.
        let Some(crate::sqlly_directive::Directive::Format { body }) =
            crate::sqlly_directive::classify(line.trim())
        else {
            continue;
        };
        if let Some(hint) = parse_body(body) {
            if let Some(existing) = hints
                .iter_mut()
                .find(|existing| existing.column.eq_ignore_ascii_case(&hint.column))
            {
                // Lines for one column merge: a later rule replaces the
                // earlier rule, a later width the earlier width.
                existing.column = hint.column;
                if hint.format != QueryColumnFormat::Keep {
                    existing.format = hint.format;
                }
                if hint.width_dp.is_some() {
                    existing.width_dp = hint.width_dp;
                }
                if hint.zone.is_some() {
                    existing.zone = hint.zone;
                }
            } else {
                hints.push(hint);
            }
        }
    }
    hints
}

/// Parse one `-- sqlly format:` body (`column=<name>; <format>[; option]`).
///
/// The body is `;`-delimited, so a JSON path or template cannot contain `;`.
fn parse_body(body: &str) -> Option<QueryFormatHint> {
    let mut parts = body
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let column = parts.next()?.strip_prefix("column=")?.trim().to_string();
    if column.is_empty() {
        return None;
    }
    let mut format_part = None;
    let mut thousands = true;
    let mut style = None;
    let mut width_dp = None;
    let mut zone = None;
    for part in parts {
        if let Some(value) = part.strip_prefix("thousands=") {
            thousands = value.eq_ignore_ascii_case("true");
        } else if let Some(value) = part.strip_prefix("style=") {
            style = Some(match value.to_ascii_lowercase().as_str() {
                "fixed" => crate::preferences::NumberStyle::Fixed,
                "percent" => crate::preferences::NumberStyle::Percent,
                "bytes" => crate::preferences::NumberStyle::Bytes,
                _ => return None,
            });
        } else if let Some(value) = part.strip_prefix("width=") {
            width_dp = Some(parse_width_dp(value)?);
        } else if let Some(value) = part.strip_prefix("zone=") {
            zone = Some(DirectiveZone::parse(value)?);
        } else if format_part.is_none() {
            format_part = Some(part);
        }
    }
    let Some(format_part) = format_part else {
        // Options alone are a rule only when they say something.
        if width_dp.is_none() && zone.is_none() {
            return None;
        }
        return Some(QueryFormatHint {
            column,
            format: QueryColumnFormat::Keep,
            width_dp,
            zone,
        });
    };
    let format = if let Some(value) = format_part.strip_prefix("json=") {
        QueryColumnFormat::JsonPath {
            path: value.to_string(),
        }
    } else if let Some(value) = format_part.strip_prefix("template=") {
        QueryColumnFormat::Template {
            template: value.to_string(),
        }
    } else if let Some(value) = format_part.strip_prefix("number=") {
        let decimals: usize = value.parse().ok()?;
        match style {
            Some(style @ crate::preferences::NumberStyle::Percent)
            | Some(style @ crate::preferences::NumberStyle::Bytes) => {
                QueryColumnFormat::NumberStyle {
                    style,
                    decimals: u8::try_from(decimals).ok()?,
                    thousands,
                }
            }
            Some(crate::preferences::NumberStyle::Fixed) | None => QueryColumnFormat::Number {
                decimals,
                thousands,
            },
        }
    } else if let Some(value) = format_part.strip_prefix("date=") {
        QueryColumnFormat::Date {
            format: value.to_string(),
        }
    } else if let Some(value) = format_part.strip_prefix("boolean=") {
        let (true_text, false_text) = value.split_once('/')?;
        QueryColumnFormat::Boolean {
            true_text: true_text.to_string(),
            false_text: false_text.to_string(),
        }
    } else if let Some(value) = format_part.strip_prefix("binary=") {
        QueryColumnFormat::Binary(match value.to_ascii_lowercase().as_str() {
            "hex" => BinaryFormat::Hex,
            "base64" => BinaryFormat::Base64,
            "uuid" => BinaryFormat::Uuid,
            "ulid" => BinaryFormat::Ulid,
            _ => return None,
        })
    } else if let Some(value) = format_part.strip_prefix("mask=") {
        let (prefix, suffix) = value.split_once('/')?;
        QueryColumnFormat::Mask {
            keep_prefix: prefix.parse().ok()?,
            keep_suffix: suffix.parse().ok()?,
        }
    } else if let Some(value) = format_part.strip_prefix("epoch=") {
        QueryColumnFormat::Epoch {
            unit: match value.to_ascii_lowercase().as_str() {
                "auto" => crate::preferences::EpochUnit::Auto,
                "s" | "seconds" => crate::preferences::EpochUnit::Seconds,
                "ms" | "milliseconds" => crate::preferences::EpochUnit::Milliseconds,
                _ => return None,
            },
        }
    } else {
        let value = format_part.strip_prefix("text=")?;
        QueryColumnFormat::Text(match value.to_ascii_lowercase().as_str() {
            "upper" => TextCase::Upper,
            "lower" => TextCase::Lower,
            "title" => TextCase::Title,
            "none" => TextCase::None,
            _ => return None,
        })
    };
    Some(QueryFormatHint {
        column,
        format,
        width_dp,
        zone,
    })
}

/// `40dp` or `40`: a positive whole number of dp up to [`MAX_WIDTH_DP`].
fn parse_width_dp(value: &str) -> Option<u32> {
    let value = value.trim();
    let digits = value
        .strip_suffix("dp")
        .or_else(|| value.strip_suffix("DP"))
        .unwrap_or(value)
        .trim();
    let dp: u32 = digits.parse().ok()?;
    (1..=MAX_WIDTH_DP).contains(&dp).then_some(dp)
}

/// The `-- sqlly format:` line that reproduces `hint` (the inverse of
/// [`parse`]); `None` when it would say nothing.
pub fn directive_line(hint: &QueryFormatHint) -> Option<String> {
    use crate::preferences::{EpochUnit, NumberStyle};
    let mut parts = vec![format!("column={}", hint.column)];
    match &hint.format {
        QueryColumnFormat::Number {
            decimals,
            thousands,
        } => {
            parts.push(format!("number={decimals}"));
            if !thousands {
                parts.push("thousands=false".into());
            }
        }
        QueryColumnFormat::NumberStyle {
            style,
            decimals,
            thousands,
        } => {
            parts.push(format!("number={decimals}"));
            parts.push(
                match style {
                    NumberStyle::Fixed => "style=fixed",
                    NumberStyle::Percent => "style=percent",
                    NumberStyle::Bytes => "style=bytes",
                }
                .into(),
            );
            if !thousands {
                parts.push("thousands=false".into());
            }
        }
        QueryColumnFormat::Date { format } => parts.push(format!("date={format}")),
        QueryColumnFormat::Boolean {
            true_text,
            false_text,
        } => parts.push(format!("boolean={true_text}/{false_text}")),
        QueryColumnFormat::Binary(BinaryFormat::Hex) => parts.push("binary=hex".into()),
        QueryColumnFormat::Binary(BinaryFormat::Base64) => parts.push("binary=base64".into()),
        QueryColumnFormat::Binary(BinaryFormat::Uuid) => parts.push("binary=uuid".into()),
        QueryColumnFormat::Binary(BinaryFormat::Ulid) => parts.push("binary=ulid".into()),
        QueryColumnFormat::Text(case) => parts.push(
            match case {
                TextCase::Upper => "text=upper",
                TextCase::Lower => "text=lower",
                TextCase::Title => "text=title",
                TextCase::None => "text=none",
            }
            .into(),
        ),
        QueryColumnFormat::Mask {
            keep_prefix,
            keep_suffix,
        } => parts.push(format!("mask={keep_prefix}/{keep_suffix}")),
        QueryColumnFormat::Epoch { unit } => parts.push(
            match unit {
                EpochUnit::Auto => "epoch=auto",
                EpochUnit::Seconds => "epoch=s",
                EpochUnit::Milliseconds => "epoch=ms",
            }
            .into(),
        ),
        QueryColumnFormat::JsonPath { path } => parts.push(format!("json={path}")),
        QueryColumnFormat::Template { template } => parts.push(format!("template={template}")),
        QueryColumnFormat::Keep => {}
    }
    if let Some(zone) = &hint.zone {
        parts.push(format!("zone={}", zone.directive_value()));
    }
    if let Some(dp) = hint.width_dp {
        parts.push(format!("width={dp}dp"));
    }
    (parts.len() > 1).then(|| format!("-- sqlly format: {}", parts.join("; ")))
}

/// The `width=` a query sets for `column`, in dp.
pub fn width_dp_for_column(column: &str, hints: &[QueryFormatHint]) -> Option<u32> {
    hints
        .iter()
        .find(|hint| hint.column.eq_ignore_ascii_case(column))
        .and_then(|hint| hint.width_dp)
}

pub fn apply_to_grid_config(
    config: &mut GridConfig,
    columns: &[ResultColumn],
    hints: &[QueryFormatHint],
) {
    config
        .column_overrides
        .resize(columns.len(), ColumnOverride::default());
    let default_number = config.default_number;
    let default_date = config.default_date.clone();
    for hint in hints {
        let Some(index) = columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(&hint.column))
        else {
            continue;
        };
        let override_format = &mut config.column_overrides[index];
        match &hint.format {
            QueryColumnFormat::Number {
                decimals,
                thousands,
            } => {
                // Places and grouping come from the directive; the grouping
                // and decimal characters, negative style, and alignment stay
                // the column's configured ones. Fixed places means no metric
                // reduction.
                let base = override_format.number.unwrap_or(default_number);
                override_format.number = Some(NumberFormat {
                    decimals: *decimals,
                    thousands_separator: *thousands,
                    metric: None,
                    ..base
                });
            }
            QueryColumnFormat::Date { format } => {
                // Date and datetime cells reach the grid as text, so the
                // pattern itself is applied when the cells are converted
                // (`grid_bridge`); this keeps the column's alignment and
                // records the pattern for the grid's own date cells.
                let base = override_format
                    .date
                    .clone()
                    .unwrap_or_else(|| default_date.clone());
                override_format.date = Some(DateFormat {
                    format: format.clone(),
                    ..base
                });
            }
            QueryColumnFormat::Boolean {
                true_text,
                false_text,
            } => {
                override_format.boolean = Some(BooleanFormat {
                    true_text: true_text.clone(),
                    false_text: false_text.clone(),
                    ..BooleanFormat::default()
                });
            }
            QueryColumnFormat::Text(case) => {
                // Keep the column's configured alignment (a UUID column's
                // Unique ID alignment, for one).
                let base = override_format.string.clone().unwrap_or_default();
                override_format.string = Some(StringFormat {
                    case: *case,
                    ..base
                });
            }
            QueryColumnFormat::Binary(_)
            | QueryColumnFormat::Mask { .. }
            | QueryColumnFormat::Epoch { .. }
            | QueryColumnFormat::JsonPath { .. }
            | QueryColumnFormat::Template { .. }
            | QueryColumnFormat::NumberStyle { .. }
            | QueryColumnFormat::Keep => {}
        }
    }
}

/// The `date=` directive pattern for `column`, when the query sets one.
pub fn date_pattern_for_column<'a>(
    column: &ResultColumn,
    hints: &'a [QueryFormatHint],
) -> Option<&'a str> {
    hints.iter().find_map(|hint| {
        if !hint.column.eq_ignore_ascii_case(&column.name) {
            return None;
        }
        match &hint.format {
            QueryColumnFormat::Date { format } => Some(format.as_str()),
            _ => None,
        }
    })
}

pub fn binary_format_for_column(
    column: &ResultColumn,
    hints: &[QueryFormatHint],
) -> Option<BinaryFormat> {
    hints.iter().find_map(|hint| {
        if !hint.column.eq_ignore_ascii_case(&column.name) {
            return None;
        }
        match hint.format {
            QueryColumnFormat::Binary(format) => Some(format),
            _ => None,
        }
    })
}

/// Mask/Epoch query hints become a display `ColumnFormat` that overrides prefs.
pub fn display_format_for_column(
    column: &ResultColumn,
    hints: &[QueryFormatHint],
) -> Option<crate::preferences::ColumnFormat> {
    use crate::preferences::{
        ColumnFormat, EpochFormat, JsonPathFormat, MaskFormat, NumberStyleFormat, ResultKind,
        TemplateFormat,
    };
    hints.iter().find_map(|hint| {
        if !hint.column.eq_ignore_ascii_case(&column.name) {
            return None;
        }
        match &hint.format {
            QueryColumnFormat::Mask {
                keep_prefix,
                keep_suffix,
            } => Some(ColumnFormat {
                kind: ResultKind::Mask,
                mask_format: MaskFormat {
                    keep_prefix: *keep_prefix,
                    keep_suffix: *keep_suffix,
                    ..MaskFormat::default()
                },
                ..ColumnFormat::default()
            }),
            QueryColumnFormat::Epoch { unit } => Some(ColumnFormat {
                kind: ResultKind::Epoch,
                epoch_format: EpochFormat {
                    unit: *unit,
                    // An epoch is an absolute instant: "as stored" is UTC.
                    time_zone_mode: hint.zone.as_ref().map(|zone| match zone.mode {
                        crate::preferences::DateTimeZoneMode::NativeAsStored => {
                            crate::preferences::DateTimeZoneMode::AlwaysUtc
                        }
                        ref mode => mode.clone(),
                    }),
                    time_zone_identifier: hint
                        .zone
                        .as_ref()
                        .map(|zone| zone.identifier.clone())
                        .unwrap_or_else(|| EpochFormat::default().time_zone_identifier),
                    ..EpochFormat::default()
                },
                ..ColumnFormat::default()
            }),
            QueryColumnFormat::JsonPath { path } => Some(ColumnFormat {
                kind: ResultKind::JsonPath,
                json_path_format: JsonPathFormat {
                    path: path.clone(),
                    ..JsonPathFormat::default()
                },
                ..ColumnFormat::default()
            }),
            QueryColumnFormat::Template { template } => Some(ColumnFormat {
                kind: ResultKind::Template,
                template_format: TemplateFormat {
                    template: template.clone(),
                },
                ..ColumnFormat::default()
            }),
            QueryColumnFormat::NumberStyle {
                style,
                decimals,
                thousands,
            } => Some(ColumnFormat {
                kind: ResultKind::Number,
                number_style_format: NumberStyleFormat {
                    style: *style,
                    decimals: *decimals,
                    use_thousands_separators: *thousands,
                },
                ..ColumnFormat::default()
            }),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_name_based_override_for_each_column() {
        let hints = parse(
            "-- sqlly format: column=Amount; number=2; thousands=false\n\
             -- sqlly format: column=CreatedAt; date=%Y-%m-%d\n\
             -- sqlly format: column=amount; number=4; thousands=true\nSELECT 1",
        );
        assert_eq!(hints.len(), 2);
        assert_eq!(
            hints[0],
            QueryFormatHint {
                column: "amount".into(),
                format: QueryColumnFormat::Number {
                    decimals: 4,
                    thousands: true,
                },
                width_dp: None,
                zone: None,
            }
        );
    }

    #[test]
    fn applies_override_by_column_name_not_position() {
        let columns = vec![
            ResultColumn {
                name: "Other".into(),
                data_type: "decimal".into(),
                nullable: false,
            },
            ResultColumn {
                name: "Amount".into(),
                data_type: "decimal".into(),
                nullable: false,
            },
        ];
        let mut config = GridConfig::default();
        apply_to_grid_config(
            &mut config,
            &columns,
            &[QueryFormatHint {
                column: "Amount".into(),
                format: QueryColumnFormat::Number {
                    decimals: 6,
                    thousands: false,
                },
                width_dp: None,
                zone: None,
            }],
        );
        assert!(config.column_overrides[0].number.is_none());
        let amount = config.column_overrides[1].number.expect("Amount override");
        assert_eq!(amount.decimals, 6);
        assert!(!amount.thousands_separator);
    }

    #[test]
    fn zone_option_parses_round_trips_and_rejects_typos() {
        use crate::preferences::DateTimeZoneMode;
        let hints = parse(
            "-- sqlly format: column=Ts; epoch=s; zone=europe/berlin\n\
             -- sqlly format: column=Created; date=%Y-%m-%d %H:%M; zone=local\n\
             -- sqlly format: column=Logged; zone=stored\n\
             -- sqlly format: column=Bad; date=%Y; zone=Mars/Olympus\nSELECT 1",
        );
        assert_eq!(hints.len(), 3, "a zone typo drops the line: {hints:?}");
        let ts = hints[0].zone.clone().expect("zone on epoch rule");
        assert_eq!(ts.mode, DateTimeZoneMode::AlwaysInTimeZone);
        assert_eq!(ts.identifier, "Europe/Berlin");
        assert_eq!(
            hints[1].zone.as_ref().map(|z| z.mode.clone()),
            Some(DateTimeZoneMode::AlwaysLocal)
        );
        assert_eq!(hints[2].format, QueryColumnFormat::Keep);
        assert_eq!(
            hints[2].zone.as_ref().map(|z| z.mode.clone()),
            Some(DateTimeZoneMode::NativeAsStored)
        );
        for hint in &hints {
            let line = directive_line(hint).expect("line");
            assert_eq!(parse(&line), vec![hint.clone()], "{line}");
        }
        assert_eq!(
            directive_line(&hints[0]).as_deref(),
            Some("-- sqlly format: column=Ts; epoch=s; zone=Europe/Berlin")
        );
        // The epoch display format carries the zone; "stored" pins UTC.
        let column = ResultColumn {
            name: "ts".into(),
            data_type: "bigint".into(),
            nullable: true,
        };
        let fmt = display_format_for_column(&column, &hints).expect("epoch");
        assert_eq!(
            fmt.epoch_format.time_zone_mode,
            Some(DateTimeZoneMode::AlwaysInTimeZone)
        );
        assert_eq!(fmt.epoch_format.time_zone_identifier, "Europe/Berlin");
        let stored = parse("-- sqlly format: column=ts; epoch=ms; zone=stored");
        let fmt = display_format_for_column(&column, &stored).expect("epoch");
        assert_eq!(
            fmt.epoch_format.time_zone_mode,
            Some(DateTimeZoneMode::AlwaysUtc)
        );
        assert_eq!(directive_zone_for("TS", &hints), hints[0].zone);
    }

    /// Sequel Ace #2148: `binary=uuid` and `binary=ulid` parse and write back.
    #[test]
    fn parses_and_round_trips_uuid_and_ulid_binary_hints() {
        let hints = parse(
            "-- sqlly format: column=Id; binary=uuid\n\
             -- sqlly format: column=EventId; binary=ULID\nSELECT 1",
        );
        assert_eq!(hints.len(), 2);
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::Binary(BinaryFormat::Uuid)
        );
        assert_eq!(
            hints[1].format,
            QueryColumnFormat::Binary(BinaryFormat::Ulid)
        );
        assert_eq!(
            directive_line(&hints[0]).as_deref(),
            Some("-- sqlly format: column=Id; binary=uuid")
        );
        assert_eq!(
            directive_line(&hints[1]).as_deref(),
            Some("-- sqlly format: column=EventId; binary=ulid")
        );
        assert!(parse("-- sqlly format: column=Id; binary=guid\nSELECT 1").is_empty());
    }

    #[test]
    fn parses_mask_and_epoch_hints() {
        let hints = parse(
            "-- sqlly format: column=Card; mask=0/4\n\
             -- sqlly format: column=Ts; epoch=s\nSELECT 1",
        );
        assert_eq!(hints.len(), 2);
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::Mask {
                keep_prefix: 0,
                keep_suffix: 4,
            }
        );
        assert_eq!(
            hints[1].format,
            QueryColumnFormat::Epoch {
                unit: crate::preferences::EpochUnit::Seconds,
            }
        );
        let col = ResultColumn {
            name: "Card".into(),
            data_type: "nvarchar".into(),
            nullable: true,
        };
        let fmt = display_format_for_column(&col, &hints).expect("mask hint");
        assert_eq!(fmt.kind, crate::preferences::ResultKind::Mask);
        assert_eq!(fmt.mask_format.keep_suffix, 4);
    }

    #[test]
    fn parse_json_path_directive() {
        let hints = parse("-- sqlly format: column=payload; json=$.customer.name\nSELECT 1");
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::JsonPath {
                path: "$.customer.name".into(),
            }
        );
    }

    #[test]
    fn parse_template_directive() {
        let hints = parse("-- sqlly format: column=name; template={value|upper}\nSELECT 1");
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::Template {
                template: "{value|upper}".into(),
            }
        );
    }

    #[test]
    fn parse_number_style_percent() {
        let hints = parse(
            "-- sqlly format: column=rate; number=1; style=percent; thousands=false\nSELECT 1",
        );
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::NumberStyle {
                style: crate::preferences::NumberStyle::Percent,
                decimals: 1,
                thousands: false,
            }
        );
    }

    #[test]
    fn parse_number_without_style_stays_number() {
        let hints = parse("-- sqlly format: column=amount; number=2; thousands=true\nSELECT 1");
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::Number {
                decimals: 2,
                thousands: true,
            }
        );
    }

    #[test]
    fn display_format_for_column_maps_new_kinds() {
        let col = ResultColumn {
            name: "payload".into(),
            data_type: "nvarchar".into(),
            nullable: true,
        };
        let hints = parse(
            "-- sqlly format: column=payload; json=$.a\n\
             -- sqlly format: column=label; template={value|upper}\n\
             -- sqlly format: column=rate; number=2; style=percent\nSELECT 1",
        );
        let json = display_format_for_column(&col, &hints).expect("json hint");
        assert_eq!(json.kind, crate::preferences::ResultKind::JsonPath);
        assert_eq!(json.json_path_format.path, "$.a");
        let label = ResultColumn {
            name: "label".into(),
            data_type: "nvarchar".into(),
            nullable: true,
        };
        let tmpl = display_format_for_column(&label, &hints).expect("template hint");
        assert_eq!(tmpl.kind, crate::preferences::ResultKind::Template);
        let rate = ResultColumn {
            name: "rate".into(),
            data_type: "decimal".into(),
            nullable: true,
        };
        let pct = display_format_for_column(&rate, &hints).expect("percent hint");
        assert_eq!(pct.kind, crate::preferences::ResultKind::Number);
        assert_eq!(
            pct.number_style_format.style,
            crate::preferences::NumberStyle::Percent
        );
    }
}
