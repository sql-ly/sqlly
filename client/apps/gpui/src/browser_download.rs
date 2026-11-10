//! Browser (wasm) build: "save to file" becomes a browser download.
//!
//! gpui's web platform has no save dialog (`prompt_for_new_path` always
//! errors there), so file exports hand their bytes to the browser instead: a
//! `Blob` behind a temporary object URL, clicked through a hidden
//! `<a download>`. The browser then saves it to the user's Downloads folder
//! (or asks where, per its own settings) under `filename`.

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;

/// Download `bytes` as a file named `filename`. `Err` carries a message fit
/// for the Messages pane when the DOM refuses any step.
pub fn download_bytes(filename: &str, bytes: &[u8]) -> Result<(), String> {
    let window = web_sys::window().ok_or_else(|| {
        crate::localization::format("browser.download.no_window", &[("filename", filename)])
    })?;
    let document = window.document().ok_or_else(|| {
        crate::localization::format("browser.download.no_document", &[("filename", filename)])
    })?;

    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let props = web_sys::BlobPropertyBag::new();
    props.set_type(mime_for(filename));
    let blob =
        web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &props).map_err(|err| {
            crate::localization::format(
                "browser.download.build_failed",
                &[
                    ("filename", filename.to_string()),
                    ("error", format!("{err:?}")),
                ],
            )
        })?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|err| {
        crate::localization::format(
            "browser.download.file_refused",
            &[
                ("filename", filename.to_string()),
                ("error", format!("{err:?}")),
            ],
        )
    })?;

    let anchor = document
        .create_element("a")
        .map_err(|err| {
            crate::localization::format(
                "browser.download.link_failed",
                &[
                    ("filename", filename.to_string()),
                    ("error", format!("{err:?}")),
                ],
            )
        })?
        .unchecked_into::<web_sys::HtmlAnchorElement>();
    anchor.set_href(&url);
    anchor.set_download(filename);
    // Some browsers only honor a click on a link that is in the document.
    let body = document.body();
    if let Some(body) = &body {
        let _ = body.append_child(&anchor);
    }
    anchor.click();
    if let Some(body) = &body {
        let _ = body.remove_child(&anchor);
    }

    // Revoking in the same tick can cancel the download in some browsers;
    // release the blob once the click has been handled.
    let revoke = Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    let _ = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 60_000);
    Ok(())
}

/// A content type for the export extensions SQLly writes; anything else is
/// an opaque binary download (the `download` attribute keeps the name).
fn mime_for(filename: &str) -> &'static str {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "csv" => "text/csv;charset=utf-8",
        "tsv" => "text/tab-separated-values;charset=utf-8",
        "json" => "application/json;charset=utf-8",
        "jsonl" => "application/jsonl;charset=utf-8",
        "md" => "text/markdown;charset=utf-8",
        "html" => "text/html;charset=utf-8",
        "xml" => "application/xml;charset=utf-8",
        "sql" | "txt" => "text/plain;charset=utf-8",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}
