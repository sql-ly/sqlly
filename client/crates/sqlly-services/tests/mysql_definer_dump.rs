//! DEFINER rewriting against real mysqldump 8.4 output (view, procedure,
//! function, trigger, event, and table data that merely mentions DEFINER).

use sqlly_services::mysql_definer::{DefinerMode, rewrite_definers};

fn fixture() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mysqldump_with_definers.sql");
    std::fs::read_to_string(path)
        .expect("fixture")
        .replace("\r\n", "\n")
}

/// Lines that are table data, where DEFINER text is a string literal.
fn is_data(line: &str) -> bool {
    line.starts_with("INSERT INTO")
}

#[test]
fn strip_removes_every_definer_from_real_mysqldump_output() {
    let dump = fixture();
    let (out, count) = rewrite_definers(&dump, DefinerMode::Strip);
    assert_eq!(count, 5, "view, procedure, function, trigger and event");
    for line in out.lines().filter(|l| !is_data(l)) {
        assert!(!line.contains("DEFINER="), "left behind: {line}");
    }
    assert!(out.contains("/*!50013 SQL SECURITY DEFINER */"));
    assert!(out.contains("/*!50003 CREATE*/ /*!50003 TRIGGER `trg`"));
    assert!(out.contains("/*!50106 CREATE*/ /*!50106 EVENT `e`"));
    assert!(out.contains("CREATE FUNCTION `f`() RETURNS int"));
    assert!(out.contains("CREATE PROCEDURE `p`()"));
    // Table data and line numbering are untouched.
    let data_in: Vec<&str> = dump.lines().filter(|l| is_data(l)).collect();
    let data_out: Vec<&str> = out.lines().filter(|l| is_data(l)).collect();
    assert_eq!(data_in, data_out);
    assert!(data_out[0].contains("DEFINER=`owner`@`%`"));
    assert_eq!(out.lines().count(), dump.lines().count());
}

#[test]
fn real_mysqldump_output_splits_into_whole_statements() {
    let (out, _) = rewrite_definers(&fixture(), DefinerMode::Strip);
    let mut splitter =
        sqlly_lang::sqllex::StreamSplitter::new(sqlly_lang::sqlmodel::Dialect::MySql);
    let mut units = Vec::new();
    splitter.push(&out, &mut units);
    splitter.finish(&mut units).expect("splits");
    let texts: Vec<&str> = units.iter().map(|u| u.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .all(|t| !t.to_ascii_uppercase().starts_with("DELIMITER")),
        "{texts:#?}"
    );
    let trigger = texts
        .iter()
        .find(|t| t.contains("TRIGGER `trg`"))
        .expect("trigger unit");
    assert!(trigger.contains("END */"), "{trigger}");
    assert!(texts.iter().any(|t| t.contains("/*!50001 VIEW `v`")));
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("CREATE PROCEDURE `p`()"))
    );
}

#[test]
fn current_user_remaps_every_definer_in_real_mysqldump_output() {
    let dump = fixture();
    let (out, count) = rewrite_definers(&dump, DefinerMode::CurrentUser);
    assert_eq!(count, 5);
    for line in out.lines().filter(|l| !is_data(l)) {
        assert!(!line.contains("`owner`"), "left behind: {line}");
    }
    assert_eq!(out.matches("DEFINER=CURRENT_USER").count(), 5);
}
