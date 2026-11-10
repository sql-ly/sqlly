//! Browser (wasm) variant of [`crate::app_data_reset`]. The fresh-install
//! reset removes filesystem roots the browser build does not have; the
//! feature is desktop-only and fails with an explicit notice.

/// Browser build: the fresh-install reset is desktop-only.
pub fn schedule_reset(credential_result: Result<(), String>) -> Result<(), String> {
    credential_result?;
    Err(crate::localization::format(
        "shell.commands.reset_app_data_browser_hint",
        &[(
            "notice",
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.commands.reset_app_data_unavailable",
            )),
        )],
    ))
}

/// No pending filesystem reset can exist in the browser build.
#[allow(dead_code)]
pub fn finish_pending_reset() -> Result<(), String> {
    Ok(())
}
