//! Notebook files in the browser.
//!
//! The host page's `notebook-files.js` (installed as
//! `globalThis.sqllyNotebookFiles`) keeps a notebooks folder in the origin's
//! storage: the Origin Private File System where the browser can write it
//! from the page, IndexedDB otherwise, memory as a last resort. It reads the
//! whole folder into memory before the app starts, so [`BrowserTextStore`]
//! answers synchronously, like the desktop app's file reads; writes land in
//! that copy at once and reach storage in the background, in order.
//!
//! The same bridge opens notebooks from the user's computer (the File System
//! Access picker where the browser has one, a file input otherwise) and
//! writes them back through the file handle the picker returned. Handles
//! stay on the page, named by an opaque id.
//!
//! JavaScript values are `!Send`, so the picker calls run in a `spawn_local`
//! task and reply over a oneshot channel, the same shape as
//! [`super::pglite`].

use std::future::Future;

use tokio::sync::oneshot;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::notebook::text_store::{TextStore, valid_name};

/// Where the notebooks folder is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Opfs,
    IndexedDb,
    /// Storage refused (a private window, blocked site data): notebooks
    /// last until the tab closes.
    Memory,
    /// The page has no bridge at all.
    Missing,
}

impl Backend {
    /// Whether what is written survives a reload.
    pub fn persists(self) -> bool {
        matches!(self, Backend::Opfs | Backend::IndexedDb)
    }
}

fn bridge() -> Option<JsValue> {
    js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("sqllyNotebookFiles"))
        .ok()
        .filter(JsValue::is_object)
}

fn field(value: &JsValue, name: &str) -> Option<JsValue> {
    js_sys::Reflect::get(value, &JsValue::from_str(name))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
}

fn js_text(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| field(value, "message").and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{value:?}"))
}

fn invoke(method: &str, args: &[JsValue]) -> Result<JsValue, String> {
    let bridge = bridge().ok_or_else(|| "browser storage is not available".to_string())?;
    let function: js_sys::Function = field(&bridge, method)
        .and_then(|f| f.dyn_into().ok())
        .ok_or_else(|| format!("the notebook files bridge has no {method}"))?;
    let args: js_sys::Array = args.iter().collect();
    function.apply(&bridge, &args).map_err(|e| js_text(&e))
}

/// Where this page keeps notebooks.
pub fn backend() -> Backend {
    match invoke("backend", &[])
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
    {
        Some("opfs") => Backend::Opfs,
        Some("indexeddb") => Backend::IndexedDb,
        Some(_) => Backend::Memory,
        None => Backend::Missing,
    }
}

/// The notebooks folder in browser storage.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrowserTextStore;

fn checked(name: &str) -> Result<JsValue, String> {
    if valid_name(name) {
        Ok(JsValue::from_str(name))
    } else {
        Err(format!("invalid storage name {name:?}"))
    }
}

impl TextStore for BrowserTextStore {
    fn read(&self, name: &str) -> Result<Option<String>, String> {
        Ok(invoke("read", &[checked(name)?])?.as_string())
    }

    fn write(&self, name: &str, text: &str) -> Result<(), String> {
        invoke("write", &[checked(name)?, JsValue::from_str(text)]).map(|_| ())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        invoke("remove", &[checked(name)?]).map(|_| ())
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        invoke("list", &[JsValue::from_str(prefix)])
            .ok()
            .and_then(|v| v.dyn_into::<js_sys::Array>().ok())
            .map(|names| names.iter().filter_map(|n| n.as_string()).collect())
            .unwrap_or_default()
    }

    fn modified(&self, name: &str) -> Option<i64> {
        invoke("modified", &[checked(name).ok()?])
            .ok()?
            .as_f64()
            .map(|ms| ms as i64)
    }
}

/// Why opening or saving a file on the user's computer failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The file is larger than a notebook may be.
    TooLarge {
        limit_mb: u64,
    },
    /// The browser did not allow writing to the file.
    Permission,
    /// The page no longer holds the file's handle (it was reloaded).
    NoHandle,
    Failed(String),
}

fn file_error(value: &JsValue) -> FileError {
    match field(value, "code").and_then(|c| c.as_string()).as_deref() {
        Some("too_large") => FileError::TooLarge {
            limit_mb: field(value, "limitMb")
                .and_then(|v| v.as_f64())
                .map(|mb| mb as u64)
                .unwrap_or(32),
        },
        Some("permission") => FileError::Permission,
        Some("no_handle") => FileError::NoHandle,
        _ => FileError::Failed(js_text(value)),
    }
}

async fn call(method: &'static str, args: Vec<JsValue>) -> Result<JsValue, FileError> {
    let promise: js_sys::Promise = invoke(method, &args)
        .map_err(FileError::Failed)?
        .dyn_into()
        .map_err(|_| {
            FileError::Failed(format!(
                "sqllyNotebookFiles.{method} did not return a Promise"
            ))
        })?;
    JsFuture::from(promise).await.map_err(|e| file_error(&e))
}

fn on_page<T: Send + 'static>(
    work: impl Future<Output = Result<T, FileError>> + 'static,
) -> impl Future<Output = Result<T, FileError>> + Send {
    let (tx, rx) = oneshot::channel();
    spawn_local(async move {
        let _ = tx.send(work.await);
    });
    async move {
        rx.await.unwrap_or_else(|_| {
            Err(FileError::Failed(
                "the browser file request was dropped".into(),
            ))
        })
    }
}

fn string_field(value: &JsValue, name: &str) -> Option<String> {
    field(value, name).and_then(|v| v.as_string())
}

/// A notebook file the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedFile {
    pub name: String,
    pub text: String,
    /// Writes go back to this file while the page holds it.
    pub handle: Option<String>,
}

/// Ask the user for a notebook file. `None` when they cancelled.
/// `description` names the file type in the picker.
pub fn pick_file(
    description: String,
) -> impl Future<Output = Result<Option<PickedFile>, FileError>> + Send {
    on_page(async move {
        let picked = call("pickFile", vec![JsValue::from_str(&description)]).await?;
        if picked.is_null() || picked.is_undefined() {
            return Ok(None);
        }
        Ok(Some(PickedFile {
            name: string_field(&picked, "name").unwrap_or_default(),
            text: string_field(&picked, "text").unwrap_or_default(),
            handle: string_field(&picked, "handle"),
        }))
    })
}

/// Whether the browser can save to a file the user chooses (the File System
/// Access API). Without it a save is a download.
pub fn can_save_files() -> bool {
    invoke("canSaveFiles", &[])
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// A file the user chose to save to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFile {
    pub name: String,
    pub handle: String,
}

/// Ask the user where to save a notebook (the browser's save picker),
/// without writing anything yet. `None` when they cancelled.
pub fn choose_save_file(
    suggested_name: String,
    description: String,
) -> impl Future<Output = Result<Option<SavedFile>, FileError>> + Send {
    on_page(async move {
        let chosen = call(
            "chooseSaveFile",
            vec![
                JsValue::from_str(&suggested_name),
                JsValue::from_str(&description),
            ],
        )
        .await?;
        if chosen.is_null() || chosen.is_undefined() {
            return Ok(None);
        }
        let handle = string_field(&chosen, "handle")
            .ok_or_else(|| FileError::Failed("the browser returned no file".into()))?;
        Ok(Some(SavedFile {
            name: string_field(&chosen, "name").unwrap_or(suggested_name),
            handle,
        }))
    })
}

/// Write `text` to the file behind `handle`. With `ask`, the browser may ask
/// the user for permission first; without, a file not already writable
/// fails with [`FileError::Permission`].
pub fn save_file(
    handle: String,
    text: String,
    ask: bool,
) -> impl Future<Output = Result<(), FileError>> + Send {
    on_page(async move {
        call(
            "saveFile",
            vec![
                JsValue::from_str(&handle),
                JsValue::from_str(&text),
                JsValue::from_bool(ask),
            ],
        )
        .await
        .map(|_| ())
    })
}
