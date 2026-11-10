//! The `-- sqlly conditional:` directive: one conditional-formatting rule
//! for a result column, scoped to the query it is written in.
//!
//! The rule model is the grid crate's ([`ConditionalRule`]); the dialog in
//! `conditional_formatting` saves the same rules globally by column name.
//! This module only translates a rule to and from its one-line form:
//!
//! ```text
//! -- sqlly conditional: column=<name>; when=<test>; text=#RRGGBB; fill=#RRGGBB; bold
//! -- sqlly conditional: column=<name>; scale=#RRGGBB..#RRGGBB
//! -- sqlly conditional: column=<name>; bar=#RRGGBB
//! ```
//!
//! Tests: `null`, `not null`, `> n`, `< n`, `between a and b`, `is <text>`,
//! `contains <text>`. Colors are `#RRGGBB` or `#RRGGBBAA`.

use sqlly_datatable::{
    ConditionalCondition, ConditionalKind, ConditionalRule, ConditionalStyle, RgbaColor,
};

/// Every `-- sqlly conditional:` rule in `sql` (lock blocks included), in
/// document order — which is also their priority. Lines that do not parse
/// are skipped (the editor squiggles them).
pub fn parse(sql: &str) -> Vec<ConditionalRule> {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    crate::result_layout::standalone_comments(&sql)
        .into_iter()
        .filter_map(
            |comment| match crate::sqlly_directive::classify_body(&comment.body)? {
                crate::sqlly_directive::Directive::Conditional { body } => parse_body(body).ok(),
                _ => None,
            },
        )
        .collect()
}

/// Parse a `conditional:` body (everything after the colon).
pub fn parse_body(body: &str) -> Result<ConditionalRule, String> {
    let mut column = None;
    let mut condition = None;
    let mut kind = None;
    let mut style = ConditionalStyle::default();
    for segment in body.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if segment.eq_ignore_ascii_case("bold") {
            style.bold = true;
            continue;
        }
        let Some((key, value)) = segment.split_once('=') else {
            return Err(crate::localization::format(
                "results.conditional_directive.setting_not_key_value",
                &[("setting", segment)],
            ));
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "column" if !value.is_empty() => column = Some(value.to_string()),
            "when" => condition = Some(parse_condition(value)?),
            "text" => style.foreground = Some(parse_color(value)?),
            "fill" => style.background = Some(parse_color(value)?),
            "scale" => {
                let (min, max) = value.split_once("..").ok_or_else(|| {
                    crate::localization::format(
                        "results.conditional_directive.invalid_scale",
                        &[("value", value)],
                    )
                })?;
                kind = Some(ConditionalKind::ColorScale {
                    min: parse_color(min)?,
                    max: parse_color(max)?,
                });
            }
            "bar" => {
                kind = Some(ConditionalKind::DataBar {
                    color: parse_color(value)?,
                });
            }
            other => {
                return Err(crate::localization::format(
                    "results.conditional_directive.unknown_setting",
                    &[("setting", other)],
                ));
            }
        }
    }
    let column = column
        .ok_or_else(|| crate::localization::text("results.conditional_directive.missing_column"))?;
    let kind = match (condition, kind) {
        (Some(_), Some(_)) => {
            return Err(crate::localization::text(
                "results.conditional_directive.condition_and_scale_conflict",
            ));
        }
        (Some(condition), None) => {
            if style == ConditionalStyle::default() {
                return Err(crate::localization::text(
                    "results.conditional_directive.missing_style",
                ));
            }
            ConditionalKind::Rule { condition, style }
        }
        (None, Some(kind)) => kind,
        (None, None) => {
            return Err(crate::localization::text(
                "results.conditional_directive.missing_rule_kind",
            ));
        }
    };
    Ok(ConditionalRule { column, kind })
}

fn parse_condition(value: &str) -> Result<ConditionalCondition, String> {
    let lower = value.to_ascii_lowercase();
    let number = |s: &str| {
        s.trim().parse::<f64>().map_err(|_| {
            crate::localization::format(
                "results.conditional_directive.invalid_number",
                &[("value", s.trim())],
            )
        })
    };
    if lower == "null" {
        return Ok(ConditionalCondition::IsNull);
    }
    if lower == "not null" {
        return Ok(ConditionalCondition::IsNotNull);
    }
    if let Some(rest) = value.strip_prefix('>') {
        return Ok(ConditionalCondition::GreaterThan(number(rest)?));
    }
    if let Some(rest) = value.strip_prefix('<') {
        return Ok(ConditionalCondition::LessThan(number(rest)?));
    }
    if let Some(rest) = lower.strip_prefix("between ") {
        let at = rest.find(" and ").ok_or_else(|| {
            crate::localization::format(
                "results.conditional_directive.invalid_between",
                &[("value", value)],
            )
        })?;
        return Ok(ConditionalCondition::Between(
            number(&rest[..at])?,
            number(&rest[at + 5..])?,
        ));
    }
    if lower.starts_with("is ") {
        return Ok(ConditionalCondition::EqualsText(
            value[3..].trim().to_string(),
        ));
    }
    if lower.starts_with("contains ") {
        return Ok(ConditionalCondition::ContainsText(
            value[9..].trim().to_string(),
        ));
    }
    Err(crate::localization::format(
        "results.conditional_directive.unknown_condition",
        &[("value", value)],
    ))
}

/// `#RRGGBB` or `#RRGGBBAA`.
fn parse_color(value: &str) -> Result<RgbaColor, String> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .filter(|h| matches!(h.len(), 6 | 8) && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            crate::localization::format(
                "results.conditional_directive.invalid_color",
                &[("value", value)],
            )
        })?;
    let channel = |i: usize| f32::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0)) / 255.0;
    Ok(RgbaColor::new(
        channel(0),
        channel(2),
        channel(4),
        if hex.len() == 8 { channel(6) } else { 1.0 },
    ))
}

fn color_hex(color: RgbaColor) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let rgb = format!(
        "#{:02X}{:02X}{:02X}",
        byte(color.r),
        byte(color.g),
        byte(color.b)
    );
    if byte(color.a) == 255 {
        rgb
    } else {
        format!("{rgb}{:02X}", byte(color.a))
    }
}

/// The `-- sqlly conditional:` line that reproduces `rule`; `None` for a
/// rule a line cannot carry (a `;` in its text, or a test with no style).
pub fn directive_line(rule: &ConditionalRule) -> Option<String> {
    if rule.column.contains(';') || rule.column.trim().is_empty() {
        return None;
    }
    let mut parts = vec![format!("column={}", rule.column.trim())];
    match &rule.kind {
        ConditionalKind::Rule { condition, style } => {
            let when = match condition {
                ConditionalCondition::IsNull => "null".to_string(),
                ConditionalCondition::IsNotNull => "not null".to_string(),
                ConditionalCondition::GreaterThan(n) => format!("> {n}"),
                ConditionalCondition::LessThan(n) => format!("< {n}"),
                ConditionalCondition::Between(a, b) => format!("between {a} and {b}"),
                ConditionalCondition::EqualsText(text) => format!("is {}", text.trim()),
                ConditionalCondition::ContainsText(text) => format!("contains {}", text.trim()),
            };
            if when.contains(';') {
                return None;
            }
            parts.push(format!("when={when}"));
            if let Some(color) = style.foreground {
                parts.push(format!("text={}", color_hex(color)));
            }
            if let Some(color) = style.background {
                parts.push(format!("fill={}", color_hex(color)));
            }
            if style.bold {
                parts.push("bold".into());
            }
            if parts.len() == 2 {
                return None;
            }
        }
        ConditionalKind::ColorScale { min, max } => {
            parts.push(format!("scale={}..{}", color_hex(*min), color_hex(*max)));
        }
        ConditionalKind::DataBar { color } => parts.push(format!("bar={}", color_hex(*color))),
    }
    Some(format!("-- sqlly conditional: {}", parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> RgbaColor {
        RgbaColor::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        )
    }

    #[test]
    fn every_kind_round_trips() {
        let style = ConditionalStyle {
            foreground: Some(rgb(0xB0, 0x00, 0x20)),
            background: Some(RgbaColor::new(1.0, 0.0, 0.0, 128.0 / 255.0)),
            bold: true,
        };
        let conditions = [
            ConditionalCondition::IsNull,
            ConditionalCondition::IsNotNull,
            ConditionalCondition::GreaterThan(1000.0),
            ConditionalCondition::LessThan(-2.5),
            ConditionalCondition::Between(10.0, 20.0),
            ConditionalCondition::EqualsText("Failed".into()),
            ConditionalCondition::ContainsText("time out".into()),
        ];
        let mut rules: Vec<ConditionalRule> = conditions
            .into_iter()
            .map(|condition| ConditionalRule {
                column: "Status".into(),
                kind: ConditionalKind::Rule { condition, style },
            })
            .collect();
        rules.push(ConditionalRule {
            column: "Latency".into(),
            kind: ConditionalKind::ColorScale {
                min: rgb(0x63, 0xBE, 0x7B),
                max: rgb(0xF8, 0x69, 0x6B),
            },
        });
        rules.push(ConditionalRule {
            column: "Order Total".into(),
            kind: ConditionalKind::DataBar {
                color: rgb(0x5B, 0x9B, 0xD5),
            },
        });
        for rule in rules {
            let line = directive_line(&rule).expect("line");
            let sql = format!("{line}\nSELECT 1");
            assert_eq!(parse(&sql), vec![rule], "{line}");
            assert!(
                crate::sqlly_directive::directive_diagnostics(&sql).is_empty(),
                "{line}"
            );
        }
    }

    #[test]
    fn rejects_incomplete_or_conflicting_rules() {
        for body in [
            "when=> 1; fill=#FF0000",
            "column=A; when=> 1",
            "column=A; when=> x; bold",
            "column=A; when=> 1; bar=#FF0000; bold",
            "column=A; fill=red; when=null",
            "column=A; scale=#FF0000",
            "column=A",
        ] {
            assert!(parse_body(body).is_err(), "{body}");
        }
    }

    #[test]
    fn rules_keep_document_order() {
        let rules = parse(
            "-- sqlly conditional: column=A; when=null; bold\n\
             -- sqlly conditional: column=A; when=not null; fill=#00FF00\nSELECT 1",
        );
        assert_eq!(rules.len(), 2);
        assert!(matches!(
            rules[0].kind,
            ConditionalKind::Rule {
                condition: ConditionalCondition::IsNull,
                ..
            }
        ));
    }
}
