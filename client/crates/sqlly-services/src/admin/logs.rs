pub struct ErrorLogReader;

impl ErrorLogReader {
    pub fn current_log_query() -> &'static str {
        "EXEC sp_readerrorlog 0"
    }

    pub fn agent_log_query() -> &'static str {
        "EXEC msdb.dbo.sp_readerrorlog"
    }

    pub fn log_by_index_query(log_index: i32) -> String {
        let log_index = log_index.max(0);
        format!("EXEC sp_readerrorlog {log_index}")
    }

    pub fn search_log_query(log_index: i32, search: &str) -> String {
        let log_index = log_index.max(0);
        format!(
            "EXEC sp_readerrorlog {log_index}, 1, N'{}'",
            search.replace('\'', "''")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_negative_error_log_index() {
        assert_eq!(
            ErrorLogReader::log_by_index_query(-1),
            "EXEC sp_readerrorlog 0"
        );
        assert!(ErrorLogReader::search_log_query(0, "can't").contains("can''t"));
    }
}
