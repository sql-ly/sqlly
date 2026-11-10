//! Pure builder for the Client Statistics pane rows.
//!
//! Consolidates the per-run metrics the results view already tracks — result
//! sets, rows returned, rows affected, and the server / first-response /
//! client-paint / wall-clock timing breakdown — into an ordered list of
//! labelled rows. No GPUI or I/O, so the formatting and which-rows-appear
//! rules are unit tested directly. The view layer paints these rows.

use super::render::{format_count, format_duration_ms};

/// One labelled metric row in the Client Statistics pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientStatRow {
    pub label: String,
    pub value: String,
    /// A short unit/explanatory note, or empty.
    pub note: String,
}

/// Inputs gathered from the results view for one completed run.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClientStatInputs {
    pub result_sets: usize,
    pub total_rows: usize,
    pub rows_affected: Option<u64>,
    /// Server execution time reported by the engine.
    pub server_ms: Option<u64>,
    /// Time from dispatch to the first response frame.
    pub first_response_ms: Option<u64>,
    /// Client render time for the first paint after completion.
    pub client_paint_ms: Option<u64>,
    /// End-to-end wall clock (first response + server + paint).
    pub wall_clock_ms: Option<u64>,
}

/// Build the ordered stat rows for a completed run. Timing rows that were never
/// measured are omitted rather than shown as a misleading zero.
pub(crate) fn client_stat_rows(inputs: &ClientStatInputs) -> Vec<ClientStatRow> {
    let mut rows = Vec::new();

    rows.push(ClientStatRow {
        label: crate::localization::text("results.client_statistics.result_sets_label"),
        value: format_count(inputs.result_sets),
        note: String::new(),
    });
    rows.push(ClientStatRow {
        label: crate::localization::text("results.client_statistics.rows_returned_label"),
        value: format_count(inputs.total_rows),
        note: crate::localization::text("results.client_statistics.rows_returned_note"),
    });
    if let Some(affected) = inputs.rows_affected {
        rows.push(ClientStatRow {
            label: crate::localization::text("results.client_statistics.rows_affected_label"),
            value: format_count(affected as usize),
            note: crate::localization::text("results.client_statistics.rows_affected_note"),
        });
    }
    if let Some(ms) = inputs.server_ms {
        rows.push(ClientStatRow {
            label: crate::localization::text("results.client_statistics.server_time_label"),
            value: format_duration_ms(ms),
            note: crate::localization::text("results.client_statistics.server_time_note"),
        });
    }
    if let Some(ms) = inputs.first_response_ms {
        rows.push(ClientStatRow {
            label: crate::localization::text("results.client_statistics.first_response_label"),
            value: format_duration_ms(ms),
            note: crate::localization::text("results.client_statistics.first_response_note"),
        });
    }
    if let Some(ms) = inputs.client_paint_ms {
        rows.push(ClientStatRow {
            label: crate::localization::text("results.client_statistics.client_display_label"),
            value: format_duration_ms(ms),
            note: crate::localization::text("results.client_statistics.client_display_note"),
        });
    }
    if let Some(ms) = inputs.wall_clock_ms {
        rows.push(ClientStatRow {
            label: crate::localization::text("results.client_statistics.wall_clock_label"),
            value: format_duration_ms(ms),
            note: crate::localization::text("results.client_statistics.wall_clock_note"),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_reports_sets_and_rows() {
        let rows = client_stat_rows(&ClientStatInputs {
            result_sets: 2,
            total_rows: 1500,
            ..Default::default()
        });
        assert_eq!(rows[0].label, "Result sets");
        assert_eq!(rows[0].value, "2");
        assert_eq!(rows[1].label, "Rows returned");
        assert_eq!(rows[1].value, "1,500");
        // No timings measured → only the two count rows.
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn omits_unmeasured_timings_but_includes_measured_ones() {
        let rows = client_stat_rows(&ClientStatInputs {
            result_sets: 1,
            total_rows: 3,
            rows_affected: Some(3),
            server_ms: Some(1250),
            first_response_ms: Some(40),
            client_paint_ms: None,
            wall_clock_ms: Some(1300),
        });
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Result sets",
                "Rows returned",
                "Rows affected",
                "Server execution time",
                "Time to first response",
                "Total wall-clock time",
            ]
        );
        // Duration formatting is applied.
        let server = rows
            .iter()
            .find(|r| r.label == "Server execution time")
            .unwrap();
        assert_eq!(server.value, format_duration_ms(1250));
        assert!(!rows.iter().any(|r| r.label == "Client display time"));
    }
}
