#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStyle {
    CapitalLetters { include_schema: bool },
    FirstLetter,
}

/// Words a generated alias must never be: a table alias equal to one of these
/// either fails to parse or silently changes the statement (`FROM orders or`).
/// Covers every two-letter keyword the consonant fallback can spell (`of`
/// from `offers`, `in` from `invoices`, …) plus the short keywords the
/// capital-letter and initials paths can produce (`and` from
/// `AccountNumberDetail`, `for` from `FiscalOrderRate`, …).
const RESERVED: &[&str] = &[
    "ON", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "GROUP", "ORDER",
    "HAVING", "BY", "UNION", "LIMIT", "OFFSET", "SET", "VALUES", "AS", "IN", "IS", "OR", "AND",
    "NOT", "TO", "IF", "DO", "GO", "OF", "FOR", "ALL", "ANY", "ASC", "DESC", "END", "ADD", "TOP",
    "USE", "KEY", "FROM", "CASE", "WHEN", "THEN", "ELSE", "NULL", "WITH", "INTO", "LIKE", "SOME",
    "SELECT", "APPLY", "EXCEPT", "WINDOW", "USING", "BETWEEN", "EXISTS",
];

pub fn is_reserved_alias_word(word: &str) -> bool {
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(word))
}

/// Build a collision-free alias for `qualified_name` (`schema.Table` or `Table`,
/// brackets/quotes stripped by the caller) given the aliases already used in the
/// statement. Returns `None` when no sensible alias can be formed.
pub fn suggest_alias(
    qualified_name: &str,
    style: AliasStyle,
    existing: &[String],
) -> Option<String> {
    let trimmed = qualified_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let segments: Vec<&str> = trimmed
        .split('.')
        .map(|segment| segment.trim_matches(|ch| matches!(ch, '[' | ']' | '"' | '`')))
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return None;
    }

    let mut candidate = match style {
        AliasStyle::CapitalLetters { include_schema } => {
            capital_letters_alias(&segments, include_schema).unwrap_or_default()
        }
        AliasStyle::FirstLetter => first_letter_alias(segments.last().copied()?),
    };

    if candidate.is_empty() {
        candidate = consonant_fallback(segments.last().copied()?);
    }
    if candidate.is_empty() {
        candidate = "t".to_string();
    }

    // Every path — capitals, initials, first letter, consonant fallback —
    // goes through the same reserved-word guard and collision suffixing.
    if is_reserved_alias_word(&candidate) {
        candidate.push('1');
    }

    let base_candidate = candidate.clone();
    let mut suffix = 2;
    while existing
        .iter()
        .any(|alias| alias.eq_ignore_ascii_case(&candidate))
    {
        candidate = format!("{base_candidate}{suffix}");
        suffix += 1;
    }

    Some(candidate)
}

fn capital_letters_alias(segments: &[&str], include_schema: bool) -> Option<String> {
    if include_schema {
        let alias = segments
            .iter()
            .enumerate()
            .map(|(index, segment)| segment_alias_part(segment, index + 1 < segments.len()))
            .collect::<String>();
        return if alias.is_empty() { None } else { Some(alias) };
    }

    let source_segment = segments.last().copied()?;
    let part = segment_alias_part(source_segment, false);
    if part.is_empty() { None } else { Some(part) }
}

fn segment_alias_part(segment: &str, schema_prefix: bool) -> String {
    if segment.contains('_') {
        return word_initials(segment);
    }
    let capitals: String = segment.chars().filter(|ch| ch.is_uppercase()).collect();
    if !capitals.is_empty() {
        return capitals.to_ascii_lowercase();
    }
    if schema_prefix {
        return segment
            .chars()
            .find(|ch| ch.is_ascii_alphabetic())
            .map(|ch| ch.to_ascii_lowercase().to_string())
            .unwrap_or_default();
    }
    String::new()
}

fn word_initials(source: &str) -> String {
    source
        .split('_')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.chars().find(|ch| ch.is_ascii_alphabetic()))
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn first_letter_alias(segment: &str) -> String {
    segment
        .chars()
        .find(|ch| ch.is_ascii_alphabetic())
        .map(|ch| ch.to_ascii_lowercase().to_string())
        .unwrap_or_default()
}

fn consonant_fallback(segment: &str) -> String {
    let mut chars = segment.chars().filter(|ch| ch.is_ascii_alphabetic());
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = first.to_ascii_lowercase().to_string();
    for ch in chars {
        if !matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u') {
            out.push(ch.to_ascii_lowercase());
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_list_is_superset_of_context_aliases() {
        let context_reserved = [
            "ON", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "GROUP",
            "ORDER", "HAVING", "BY", "UNION", "LIMIT", "OFFSET", "SET", "VALUES",
        ];
        for word in context_reserved {
            assert!(
                is_reserved_alias_word(word),
                "{word} should be reserved in alias.rs"
            );
        }
    }

    #[test]
    fn payment_method_caps_alias() {
        assert_eq!(
            suggest_alias(
                "PaymentMethod",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            Some("pm".to_string())
        );
    }

    #[test]
    fn sales_order_header_caps_alias() {
        assert_eq!(
            suggest_alias(
                "sales.SalesOrderHeader",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            Some("soh".to_string())
        );
    }

    #[test]
    fn sales_order_header_caps_alias_with_schema() {
        assert_eq!(
            suggest_alias(
                "sales.SalesOrderHeader",
                AliasStyle::CapitalLetters {
                    include_schema: true
                },
                &[]
            ),
            Some("ssoh".to_string())
        );
    }

    #[test]
    fn snake_case_word_initials() {
        assert_eq!(
            suggest_alias(
                "sales_order_header",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            Some("soh".to_string())
        );
    }

    #[test]
    fn lowercase_table_consonant_fallback() {
        assert_eq!(
            suggest_alias("customers", CAPS, &[]),
            Some("cs".to_string())
        );
        assert_eq!(
            suggest_alias("dbo.products", CAPS, &[]),
            Some("pr".to_string())
        );
    }

    const CAPS: AliasStyle = AliasStyle::CapitalLetters {
        include_schema: false,
    };

    #[test]
    fn consonant_fallback_never_spells_a_reserved_word() {
        // `orders` → `or` was the reported bug; the rest are the other
        // keywords the fallback (first letter + first consonant) can spell.
        for (table, alias) in [
            ("orders", "or1"),
            ("onboarding", "on1"),
            ("assets", "as1"),
            ("invoices", "in1"),
            ("issues", "is1"),
            ("bytes", "by1"),
            ("offers", "of1"),
            ("iframes", "if1"),
        ] {
            assert_eq!(
                suggest_alias(table, CAPS, &[]).as_deref(),
                Some(alias),
                "alias for {table}"
            );
            let generated = suggest_alias(table, CAPS, &[]).unwrap();
            assert!(!is_reserved_alias_word(&generated), "{table} → {generated}");
        }
    }

    #[test]
    fn capital_letters_never_spell_a_reserved_word() {
        for (table, alias) in [
            ("TableOrders", "to1"),
            ("GroupOrder", "go1"),
            ("DataObject", "do1"),
            ("IssueSummary", "is1"),
            ("AccountNumberDetail", "and1"),
            ("FiscalOrderRate", "for1"),
            ("order_number", "on1"),
        ] {
            assert_eq!(
                suggest_alias(table, CAPS, &[]).as_deref(),
                Some(alias),
                "alias for {table}"
            );
        }
    }

    #[test]
    fn reserved_fallback_alias_still_avoids_collisions() {
        assert_eq!(
            suggest_alias("orders", CAPS, &["or1".to_string()]),
            Some("or12".to_string())
        );
        assert_eq!(
            suggest_alias("dbo.orders", CAPS, &["OR1".to_string(), "or12".to_string()]),
            Some("or13".to_string())
        );
    }

    #[test]
    fn no_generated_alias_is_reserved_across_styles() {
        let names = [
            "orders",
            "onboarding",
            "assets",
            "invoices",
            "issues",
            "bytes",
            "offers",
            "Order",
            "OrderNotes",
            "sales.SalesOrderHeader",
            "a_s",
            "o_n",
            "Insert",
            "x",
        ];
        for name in names {
            for style in [
                CAPS,
                AliasStyle::CapitalLetters {
                    include_schema: true,
                },
                AliasStyle::FirstLetter,
            ] {
                let alias = suggest_alias(name, style, &[]).unwrap();
                assert!(
                    !is_reserved_alias_word(&alias),
                    "{name} ({style:?}) → reserved alias {alias}"
                );
            }
        }
    }

    #[test]
    fn single_capital_letter_alias() {
        assert_eq!(
            suggest_alias(
                "Order",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            Some("o".to_string())
        );
    }

    #[test]
    fn reserved_alias_gets_suffix() {
        assert_eq!(
            suggest_alias(
                "OrderNotes",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            Some("on1".to_string())
        );
    }

    #[test]
    fn first_letter_avoids_existing_aliases() {
        assert_eq!(
            suggest_alias(
                "dbo.Customers",
                AliasStyle::FirstLetter,
                &["c".to_string(), "c2".to_string()]
            ),
            Some("c3".to_string())
        );
    }

    #[test]
    fn empty_name_returns_none() {
        assert_eq!(
            suggest_alias(
                "",
                AliasStyle::CapitalLetters {
                    include_schema: false
                },
                &[]
            ),
            None
        );
    }
}
