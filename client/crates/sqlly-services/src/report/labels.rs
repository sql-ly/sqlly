//! The words a rendered report prints around the data (badges, headers,
//! page numbers). Services never look text up themselves: the caller
//! resolves [`ReportLabels::KEYS`] through its localization layer
//! (`ReportLabels::resolve(|key| sqlly_i18n::try_text(key))`) and passes
//! the result in; the defaults are the English catalog values.

macro_rules! labels {
    ($($field:ident => $english:literal,)*) => {
        /// Report chrome text. Values with `{name}` placeholders are filled
        /// by [`ReportLabels::fill`].
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct ReportLabels {
            $(pub $field: String,)*
        }

        impl Default for ReportLabels {
            fn default() -> Self {
                Self { $($field: $english.to_string(),)* }
            }
        }

        impl ReportLabels {
            /// Catalog keys, in field order.
            pub const KEYS: &'static [&'static str] = &[
                $(concat!("notebook.report.labels.", stringify!($field)),)*
            ];

            /// Each label from `lookup`, falling back to English for any
            /// key it does not know.
            pub fn resolve(lookup: impl Fn(&str) -> Option<String>) -> Self {
                Self {
                    $($field: lookup(concat!("notebook.report.labels.", stringify!($field)))
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| $english.to_string()),)*
                }
            }
        }
    };
}

labels! {
    generated => "Generated",
    author => "Prepared by",
    notebook => "Notebook",
    version => "Version",
    parameters => "Parameters",
    connections => "Data sources",
    environment => "Environment",
    production => "Production",
    database => "Database",
    contents => "Contents",
    page_of => "Page {page} of {pages}",
    complete => "Complete",
    truncated => "Truncated at {count} rows",
    spilled => "{count} rows in total",
    sampled => "Sample of {count} rows",
    partial => "Partial result",
    derived_partial => "Computed from partial data",
    rows_shown => "Showing {shown} of {total} rows",
    data_withheld => "Data withheld from this export",
    no_rows => "No rows",
    callout_fact => "Fact",
    callout_derived => "Derived",
    callout_hypothesis => "Hypothesis",
    callout_recommendation => "Recommendation",
    callout_warning => "Warning",
    callout_note => "Note",
    evidence => "Evidence",
    assertion_passed => "Passed",
    assertion_failed => "Failed",
    assertion_warning => "Warning",
    assertion_error => "Error",
    assertion_skipped => "Skipped",
    assertion_inconclusive => "Inconclusive",
    expected => "Expected",
    actual => "Actual",
    provenance => "Provenance",
    cell => "Cell",
    run => "Run",
    connection => "Connection",
    extracted => "Extracted",
    sql_hash => "SQL hash",
    rows => "Rows",
    appendix_sql => "Appendix: SQL",
    appendix_provenance => "Appendix: Provenance",
    compared_with => "vs {label}",
    status_good => "On track",
    status_warning => "Watch",
    status_critical => "Off track",
    status_neutral => "Neutral",
    other => "Other",
    total => "Total",
    null_value => "(null)",
    redacted => "[redacted]",
    summary_sheet => "Summary",
    sheet_contents => "Sheets",
    name_column => "Name",
    value_column => "Value",
    status => "Status",
    image_unavailable => "Image not available in this format",
    chart_unavailable => "Chart not available: {reason}",
    chart_no_data => "No data to chart",
    export_notes => "Export notes",
    kpi_target => "Target",
    kpi_baseline => "Baseline",
    kpi_previous => "Previous",
    kpi_incomplete => "Computed from incomplete data; status withheld.",
    failing_rows => "{count} failing rows",
    failing_rows_one => "{count} failing row",
    check_name => "{cell} check {number}",
    run_subtitle => "Run {run} · {started}",
    chart => "Chart",
}

impl ReportLabels {
    /// The English labels, shared.
    pub fn english() -> &'static ReportLabels {
        static ENGLISH: std::sync::OnceLock<ReportLabels> = std::sync::OnceLock::new();
        ENGLISH.get_or_init(ReportLabels::default)
    }

    /// `template` with each `{name}` replaced by its value.
    pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
        let mut out = template.to_string();
        for (name, value) in args {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }

    pub fn callout(&self, kind: super::doc::CalloutKind) -> &str {
        use super::doc::CalloutKind as K;
        match kind {
            K::Fact => &self.callout_fact,
            K::Derived => &self.callout_derived,
            K::Hypothesis => &self.callout_hypothesis,
            K::Recommendation => &self.callout_recommendation,
            K::Warning => &self.callout_warning,
            K::Note => &self.callout_note,
        }
    }

    pub fn assertion(&self, status: super::doc::AssertionStatus) -> &str {
        use super::doc::AssertionStatus as S;
        match status {
            S::Passed => &self.assertion_passed,
            S::Failed => &self.assertion_failed,
            S::Warning => &self.assertion_warning,
            S::Error => &self.assertion_error,
            S::Skipped => &self.assertion_skipped,
            S::Inconclusive => &self.assertion_inconclusive,
        }
    }

    pub fn status(&self, status: super::doc::Status) -> &str {
        use super::doc::Status as S;
        match status {
            S::Good => &self.status_good,
            S::Warning => &self.status_warning,
            S::Critical => &self.status_critical,
            S::Neutral => &self.status_neutral,
        }
    }

    /// The completeness badge for a table, `None` when complete.
    pub fn completeness(&self, completeness: super::doc::Completeness) -> Option<String> {
        use super::doc::Completeness as C;
        Some(match completeness {
            C::Complete => return None,
            C::Truncated { cap } => Self::fill(&self.truncated, &[("count", &cap.to_string())]),
            C::Spilled { total } => Self::fill(&self.spilled, &[("count", &total.to_string())]),
            C::Sampled { n } => Self::fill(&self.sampled, &[("count", &n.to_string())]),
            C::Partial => self.partial.clone(),
            C::DerivedFromPartial => self.derived_partial.clone(),
        })
    }

    pub fn page(&self, page: usize, pages: usize) -> String {
        Self::fill(
            &self.page_of,
            &[("page", &page.to_string()), ("pages", &pages.to_string())],
        )
    }

    pub fn shown(&self, shown: usize, total: u64) -> String {
        Self::fill(
            &self.rows_shown,
            &[("shown", &shown.to_string()), ("total", &total.to_string())],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_to_english() {
        let labels = ReportLabels::resolve(|key| {
            (key == "notebook.report.labels.contents").then(|| "Inhalt".to_string())
        });
        assert_eq!(labels.contents, "Inhalt");
        assert_eq!(labels.generated, "Generated");
    }

    #[test]
    fn keys_are_unique_and_namespaced() {
        let mut seen = std::collections::HashSet::new();
        for key in ReportLabels::KEYS {
            assert!(key.starts_with("notebook.report.labels."));
            assert!(seen.insert(key));
        }
    }

    #[test]
    fn placeholders_fill() {
        let labels = ReportLabels::default();
        assert_eq!(labels.page(2, 9), "Page 2 of 9");
        assert_eq!(labels.shown(50, 1200), "Showing 50 of 1200 rows");
    }
}
