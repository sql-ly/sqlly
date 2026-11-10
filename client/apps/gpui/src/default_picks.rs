//! Per-engine default-expression quick picks for the structure editor and
//! designers. Labels are what the Select shows; `expr` is inserted verbatim.
//! Callers map a confirmed Select row back by index, never by label text.

use sqlly_core::{ConnectionKind, DatabaseType};

#[derive(Clone, Copy)]
pub enum DefaultPickLabel {
    Localized(&'static str),
    Technical(&'static str),
}

impl std::fmt::Display for DefaultPickLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Localized(key) => f.write_str(&crate::localization::text(key)),
            Self::Technical(label) => f.write_str(label),
        }
    }
}

pub struct DefaultPick {
    pub label: DefaultPickLabel,
    pub expr: &'static str,
}

pub fn default_picks(db: DatabaseType) -> &'static [DefaultPick] {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => &[
            DefaultPick {
                label: DefaultPickLabel::Localized(
                    "results.default_picks.postgres_current_timestamp",
                ),
                expr: "CURRENT_TIMESTAMP",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("now()"),
                expr: "now()",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.postgres_random_uuid"),
                expr: "gen_random_uuid()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("true"),
                expr: "true",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("false"),
                expr: "false",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("0"),
                expr: "0",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.postgres_empty_string"),
                expr: "''",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.postgres_sequence"),
                expr: "nextval('')",
            },
        ],
        ConnectionKind::MySqlWire => &[
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.mysql_current_timestamp"),
                expr: "CURRENT_TIMESTAMP",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("NOW()"),
                expr: "NOW()",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.mysql_random_uuid"),
                expr: "(UUID())",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("1"),
                expr: "1",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("0"),
                expr: "0",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.mysql_empty_string"),
                expr: "''",
            },
        ],
        ConnectionKind::Tds => &[
            DefaultPick {
                label: DefaultPickLabel::Technical("SYSDATETIME()"),
                expr: "SYSDATETIME()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("GETDATE()"),
                expr: "GETDATE()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("SYSUTCDATETIME()"),
                expr: "SYSUTCDATETIME()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("NEWID()"),
                expr: "NEWID()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("NEWSEQUENTIALID()"),
                expr: "NEWSEQUENTIALID()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("1"),
                expr: "1",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("0"),
                expr: "0",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.tds_empty_string"),
                expr: "''",
            },
        ],
        ConnectionKind::OracleNet => &[
            DefaultPick {
                label: DefaultPickLabel::Technical("SYSTIMESTAMP"),
                expr: "SYSTIMESTAMP",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("SYSDATE"),
                expr: "SYSDATE",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("SYS_GUID()"),
                expr: "SYS_GUID()",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("NULL"),
                expr: "NULL",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("0"),
                expr: "0",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("''"),
                expr: "''",
            },
        ],
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp | ConnectionKind::DuckDbFile => &[
            DefaultPick {
                label: DefaultPickLabel::Technical("CURRENT_TIMESTAMP"),
                expr: "CURRENT_TIMESTAMP",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("CURRENT_DATE"),
                expr: "CURRENT_DATE",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("1"),
                expr: "1",
            },
            DefaultPick {
                label: DefaultPickLabel::Technical("0"),
                expr: "0",
            },
            DefaultPick {
                label: DefaultPickLabel::Localized("results.default_picks.sqlite_empty_string"),
                expr: "''",
            },
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    #[test]
    fn postgres_includes_gen_random_uuid() {
        let picks = default_picks(DatabaseType::Postgres);
        assert!(picks.iter().any(|p| p.expr == "gen_random_uuid()"));
    }

    #[test]
    fn tds_has_no_now() {
        let picks = default_picks(DatabaseType::SqlServer);
        assert!(!picks.iter().any(|p| p.expr == "now()"));
    }

    #[test]
    fn unknown_kind_is_empty() {
        assert!(default_picks(DatabaseType::Db2).is_empty());
        assert!(
            default_picks(DatabaseType::Oracle)
                .iter()
                .any(|p| p.expr == "SYSTIMESTAMP")
        );
    }
}
