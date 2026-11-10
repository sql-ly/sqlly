//! SQLly Embedded launch context, the pure half: turn one
//! [`LaunchOpen`] the bridge sent with its hello into what a query tab
//! needs — the buffer text, the parameter strip's seeded values and the
//! caret offset. The app half (`app/browser_proxy.rs`) connects and opens
//! the tabs.
//!
//! Launch parameters become ordinary `-- sqlly param:` declarations so the
//! strip shows them as typed inputs holding the host's values: a parameter
//! the script already declares keeps the script's own declaration (and
//! type), and every other one gets a declaration line on top of the script.
//! The statements themselves are loaded verbatim, and nothing runs until
//! the user presses Run.

use sqlly_core::credential_proxy::LaunchOpen;

use crate::query_params::{ParamSpec, ParamType, parse_param_directives};

/// What one launch tab opens with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTab {
    /// Declarations for parameters the script does not declare, then the
    /// script.
    pub text: String,
    /// The strip's rows: each launch parameter's spec and value.
    pub params: Vec<(ParamSpec, String)>,
    /// Where the caret goes (a byte offset into `text`), `None` to leave it
    /// at the start.
    pub cursor: Option<usize>,
}

/// Build the tab for `open`.
pub fn launch_tab(open: &LaunchOpen) -> LaunchTab {
    let declared = parse_param_directives(&open.script);
    let mut header = String::new();
    let mut params = Vec::with_capacity(open.parameters.len());
    for parameter in &open.parameters {
        let spec = match declared
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(&parameter.name))
        {
            Some(existing) => existing.clone(),
            None => {
                // The bridge only sends canonical type names; anything else
                // (a newer bridge) is offered as text rather than dropped.
                let ty = ParamType::parse(&parameter.ty).unwrap_or(ParamType::Text);
                header.push_str(&format!(
                    "-- sqlly param: {} {}\n",
                    parameter.name,
                    ty.label()
                ));
                ParamSpec {
                    name: parameter.name.clone(),
                    ty,
                    default: None,
                }
            }
        };
        params.push((spec, parameter.value.clone().unwrap_or_default()));
    }
    let cursor = open
        .cursor
        .map(|cursor| header.len() + cursor.offset_in(&open.script));
    LaunchTab {
        text: format!("{header}{}", open.script),
        params,
        cursor,
    }
}

/// The status line after the launch tabs opened.
pub fn opened_message(count: usize, connection: Option<&str>) -> String {
    let count_text = count.to_string();
    match (count, connection) {
        (1, Some(connection)) => crate::localization::format(
            "query.launch_tabs.opened_one_connected",
            &[("connection", connection)],
        ),
        (1, None) => crate::localization::text("query.launch_tabs.opened_one"),
        (_, Some(_)) => crate::localization::format(
            "query.launch_tabs.opened_many_connected",
            &[("count", &count_text)],
        ),
        (_, None) => {
            crate::localization::format("query.launch_tabs.opened_many", &[("count", &count_text)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::credential_proxy::{LAUNCH_PARAMETER_TYPES, LaunchCursor, LaunchParameter};

    fn parameter(name: &str, ty: &str, value: Option<&str>) -> LaunchParameter {
        LaunchParameter {
            name: name.into(),
            ty: ty.into(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn undeclared_parameters_get_declarations_and_the_cursor_follows_the_script() {
        let open = LaunchOpen {
            script: "SELECT *\nFROM orders WHERE total > @min_total AND day = @day".into(),
            parameters: vec![
                parameter("@min_total", "int", Some("500")),
                parameter("@day", "date", None),
            ],
            cursor: Some(LaunchCursor { line: 2, column: 6 }),
            ..LaunchOpen::default()
        };
        let tab = launch_tab(&open);
        assert_eq!(
            tab.text,
            "-- sqlly param: @min_total int\n-- sqlly param: @day date\nSELECT *\nFROM orders WHERE total > @min_total AND day = @day"
        );
        let cursor = tab.cursor.expect("cursor");
        assert!(
            tab.text[cursor..].starts_with("orders WHERE"),
            "{}",
            &tab.text[cursor..]
        );
        assert_eq!(tab.params.len(), 2);
        assert_eq!(tab.params[0].0.ty, ParamType::Int);
        assert_eq!(tab.params[0].1, "500");
        assert_eq!(tab.params[1].1, "", "a null value leaves the input empty");

        // The declarations are what the strip itself parses.
        let specs = parse_param_directives(&tab.text);
        assert_eq!(
            specs,
            tab.params
                .iter()
                .map(|(s, _)| s.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_parameter_the_script_declares_keeps_its_declaration() {
        let open = LaunchOpen {
            script: "-- sqlly param: @region text = 'west'\nSELECT * FROM t WHERE region = @Region"
                .into(),
            parameters: vec![parameter("@REGION", "int", Some("east"))],
            ..LaunchOpen::default()
        };
        let tab = launch_tab(&open);
        assert_eq!(tab.text, open.script, "nothing is added");
        assert_eq!(tab.params[0].0.name, "@region");
        assert_eq!(
            tab.params[0].0.ty,
            ParamType::Text,
            "the script's type wins"
        );
        assert_eq!(tab.params[0].1, "east");
        assert_eq!(tab.cursor, None);
    }

    #[test]
    fn every_launch_type_is_a_strip_type() {
        for ty in LAUNCH_PARAMETER_TYPES {
            let parsed = ParamType::parse(ty).unwrap_or_else(|e| panic!("{ty}: {e}"));
            assert_eq!(parsed.label(), *ty, "canonical spelling round-trips");
        }
    }

    #[test]
    fn messages_name_the_connection_for_a_single_tab() {
        assert_eq!(
            opened_message(1, Some("Shop")),
            "Opened a query on Shop from your app — review it before running."
        );
        assert_eq!(
            opened_message(3, Some("Shop")),
            "Opened 3 queries from your app — review before running."
        );
    }
}
