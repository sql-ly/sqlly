//! Application localization lifecycle. Catalog lookup is deliberately cheap;
//! file I/O and JSON parsing happen only at startup, Apply, or a host command.

use rust_i18n::Backend;
pub use sqlly_i18n::{LANGUAGES, Language};
use std::{
    borrow::Cow,
    sync::{
        Once,
        atomic::{AtomicU64, Ordering},
    },
};

struct SqllyComponentBackend;

const COMPONENT_KEYS: &[(&str, &str)] = &[
    ("Attachment.Remove", "components.attachment.remove"),
    ("Attachment.Retry", "components.attachment.retry"),
    ("Calendar.month.April", "components.calendar.month.april"),
    ("Calendar.month.August", "components.calendar.month.august"),
    (
        "Calendar.month.December",
        "components.calendar.month.december",
    ),
    (
        "Calendar.month.February",
        "components.calendar.month.february",
    ),
    (
        "Calendar.month.January",
        "components.calendar.month.january",
    ),
    ("Calendar.month.July", "components.calendar.month.july"),
    ("Calendar.month.June", "components.calendar.month.june"),
    ("Calendar.month.March", "components.calendar.month.march"),
    ("Calendar.month.May", "components.calendar.month.may"),
    (
        "Calendar.month.November",
        "components.calendar.month.november",
    ),
    (
        "Calendar.month.October",
        "components.calendar.month.october",
    ),
    (
        "Calendar.month.September",
        "components.calendar.month.september",
    ),
    ("Calendar.week.0", "components.calendar.week.sunday"),
    ("Calendar.week.1", "components.calendar.week.monday"),
    ("Calendar.week.2", "components.calendar.week.tuesday"),
    ("Calendar.week.3", "components.calendar.week.wednesday"),
    ("Calendar.week.4", "components.calendar.week.thursday"),
    ("Calendar.week.5", "components.calendar.week.friday"),
    ("Calendar.week.6", "components.calendar.week.saturday"),
    ("Carousel.go_to_slide", "components.carousel.go_to_slide"),
    ("Carousel.label", "components.carousel.label"),
    ("Carousel.next", "components.carousel.next"),
    ("Carousel.pagination", "components.carousel.pagination"),
    ("Carousel.previous", "components.carousel.previous"),
    ("Carousel.slide", "components.carousel.slide"),
    ("Chart.close", "components.chart.close"),
    ("Chart.high", "components.chart.high"),
    ("Chart.low", "components.chart.low"),
    ("Chart.open", "components.chart.open"),
    ("ColorPicker.Alpha", "components.color_picker.alpha"),
    ("ColorPicker.HSLA", "components.color_picker.hsla"),
    ("ColorPicker.Hue", "components.color_picker.hue"),
    ("ColorPicker.Lightness", "components.color_picker.lightness"),
    ("ColorPicker.Palette", "components.color_picker.palette"),
    (
        "ColorPicker.Saturation",
        "components.color_picker.saturation",
    ),
    ("ComboBox.empty", "components.combo_box.empty"),
    ("ComboBox.placeholder", "components.combo_box.placeholder"),
    (
        "ComboBox.search_placeholder",
        "components.combo_box.search_placeholder",
    ),
    ("Command.empty", "components.command.empty"),
    ("Command.placeholder", "components.command.placeholder"),
    (
        "DatePicker.placeholder",
        "components.date_picker.placeholder",
    ),
    ("DatePicker.time", "components.date_picker.time"),
    ("Dialog.cancel", "components.dialog.cancel"),
    ("Dialog.ok", "components.dialog.ok"),
    ("Diff.AcceptBoth", "components.diff.accept_both"),
    ("Diff.AcceptCurrent", "components.diff.accept_current"),
    ("Diff.AcceptIncoming", "components.diff.accept_incoming"),
    ("Diff.AcceptedBoth", "components.diff.accepted_both"),
    ("Diff.AcceptedCurrent", "components.diff.accepted_current"),
    ("Diff.AcceptedIncoming", "components.diff.accepted_incoming"),
    ("Diff.AddAnnotation", "components.diff.add_annotation"),
    ("Diff.AddedFile", "components.diff.added_file"),
    ("Diff.AddedLine", "components.diff.added_line"),
    ("Diff.BaseChange", "components.diff.base_change"),
    ("Diff.BinaryChanges", "components.diff.binary_changes"),
    ("Diff.CollapseFile", "components.diff.collapse_file"),
    ("Diff.ConflictedFile", "components.diff.conflicted_file"),
    ("Diff.CopiedFile", "components.diff.copied_file"),
    ("Diff.CurrentChange", "components.diff.current_change"),
    ("Diff.DeletedFile", "components.diff.deleted_file"),
    ("Diff.EmptyFile", "components.diff.empty_file"),
    ("Diff.ExpandAbove", "components.diff.expand_above"),
    ("Diff.ExpandBelow", "components.diff.expand_below"),
    ("Diff.ExpandFile", "components.diff.expand_file"),
    ("Diff.ExpandLines", "components.diff.expand_lines"),
    ("Diff.HiddenLines", "components.diff.hidden_lines"),
    ("Diff.IncomingChange", "components.diff.incoming_change"),
    ("Diff.Modified", "components.diff.modified"),
    ("Diff.NoChanges", "components.diff.no_changes"),
    ("Diff.NoNewline", "components.diff.no_newline"),
    ("Diff.NoTextChanges", "components.diff.no_text_changes"),
    ("Diff.Original", "components.diff.original"),
    ("Diff.RemovedLine", "components.diff.removed_line"),
    ("Diff.RenamedFile", "components.diff.renamed_file"),
    ("Diff.SelectLine", "components.diff.select_line"),
    ("Diff.SourceLine", "components.diff.source_line"),
    ("Diff.UnchangedLines", "components.diff.unchanged_lines"),
    ("Diff.UndoResolution", "components.diff.undo_resolution"),
    ("Diff.Viewer", "components.diff.viewer"),
    ("Diff.ViewerFiles", "components.diff.viewer_files"),
    ("Dock.Close", "components.dock.close"),
    ("Dock.Collapse", "components.dock.collapse"),
    ("Dock.Expand", "components.dock.expand"),
    ("Dock.Unnamed", "components.dock.unnamed"),
    ("Dock.Zoom In", "components.dock.zoom_in"),
    ("Dock.Zoom Out", "components.dock.zoom_out"),
    (
        "DropdownButton.more_options",
        "components.dropdown_button.more_options",
    ),
    ("Input.Copy", "components.input.copy"),
    ("Input.Cut", "components.input.cut"),
    ("Input.Decrement", "components.input.decrement"),
    (
        "Input.Go to Definition",
        "components.input.go_to_definition",
    ),
    ("Input.Increment", "components.input.increment"),
    ("Input.Paste", "components.input.paste"),
    ("Input.Replace", "components.input.replace"),
    ("Input.Replace All", "components.input.replace_all"),
    ("Input.Select All", "components.input.select_all"),
    (
        "Input.Show Code Actions",
        "components.input.show_code_actions",
    ),
    (
        "List.search_placeholder",
        "components.list.search_placeholder",
    ),
    ("Pagination.next", "components.pagination.next"),
    ("Pagination.previous", "components.pagination.previous"),
    (
        "Questionnaire.error.optional",
        "components.questionnaire.error.optional",
    ),
    (
        "Questionnaire.error.required",
        "components.questionnaire.error.required",
    ),
    ("Questionnaire.next", "components.questionnaire.next"),
    (
        "Questionnaire.previous",
        "components.questionnaire.previous",
    ),
    (
        "Questionnaire.progress",
        "components.questionnaire.progress",
    ),
    ("Questionnaire.skip", "components.questionnaire.skip"),
    ("Questionnaire.submit", "components.questionnaire.submit"),
    ("Select.placeholder", "components.select.placeholder"),
    ("Settings.Reset All", "components.settings.reset_all"),
    (
        "Settings.search_placeholder",
        "components.settings.search_placeholder",
    ),
    ("Speech.Start", "components.speech.start"),
    ("Speech.Stop", "components.speech.stop"),
    ("Speech.Unavailable", "components.speech.unavailable"),
    ("Table.SortBy", "components.table.sort_by"),
];

impl SqllyComponentBackend {
    fn catalog_key(key: &str) -> Option<&'static str> {
        let key = key.strip_prefix("gpui_component.")?;
        COMPONENT_KEYS
            .binary_search_by(|(candidate, _)| (*candidate).cmp(key))
            .ok()
            .map(|index| COMPONENT_KEYS[index].1)
    }

    fn translate_catalog(locale: &str, catalog_key: &str) -> Option<String> {
        let language = Language::from_code(locale)?;
        if sqlly_i18n::current_language() == language {
            sqlly_i18n::try_text(catalog_key)
        } else {
            sqlly_i18n::compiled_value(language, catalog_key).map(str::to_owned)
        }
    }
}

impl Backend for SqllyComponentBackend {
    fn available_locales(&self) -> Vec<Cow<'_, str>> {
        LANGUAGES
            .iter()
            .map(|info| Cow::Borrowed(info.code))
            .collect()
    }

    fn translate(&self, locale: &str, key: &str) -> Option<Cow<'_, str>> {
        Self::translate_catalog(locale, Self::catalog_key(key)?).map(Cow::Owned)
    }

    fn messages_for_locale(&self, locale: &str) -> Option<Vec<(Cow<'_, str>, Cow<'_, str>)>> {
        Language::from_code(locale)?;
        let messages = COMPONENT_KEYS
            .iter()
            .filter_map(|(component_key, catalog_key)| {
                Self::translate_catalog(locale, catalog_key).map(|value| {
                    (
                        Cow::Owned(format!("gpui_component.{component_key}")),
                        Cow::Owned(value),
                    )
                })
            })
            .collect::<Vec<_>>();
        (!messages.is_empty()).then_some(messages)
    }
}

rust_i18n::i18n!("locales", backend = SqllyComponentBackend);

static CHROME_REVISION: AtomicU64 = AtomicU64::new(0);
static EXTEND_COMPONENTS: Once = Once::new();

pub fn initialize_component_translations() {
    EXTEND_COMPONENTS.call_once(|| {
        // This is the supported expansion of rust_i18n::extend!(gpui_component).
        // The macro itself requires the backend function at the crate root;
        // SQLLY keeps its application backend scoped to this module.
        gpui_component::_rust_i18n_extend(_rust_i18n_backend(), "gpui_component");
    });
    // Hot path: the grid asks for boolean/date/pivot labels per painted cell.
    // One catalog lookup per call: compiled values are borrowed from static
    // data and copied once into the `String` the datatable `Translator` trait
    // returns; messages with arguments check the key and format together.
    sqlly_datatable::localization::install_translator(|key: &str, arguments: &[(&str, &str)]| {
        if arguments.is_empty() {
            sqlly_i18n::try_text_cow(key).map(Cow::into_owned)
        } else {
            sqlly_i18n::format_if_known(key, arguments).and_then(Result::ok)
        }
    });
    gpui_component::set_locale(sqlly_i18n::current_language().code());
}

fn sync_component_locale() {
    initialize_component_translations();
}

pub fn chrome_revision() -> u64 {
    CHROME_REVISION.load(Ordering::Acquire)
}

pub fn refresh(cx: &mut gpui::App) {
    CHROME_REVISION.fetch_add(1, Ordering::AcqRel);
    cx.set_global(LocalizationChanged);
    crate::app::AppView::refresh_all_localized_windows(cx);
}

pub(crate) struct LocalizationChanged;
impl gpui::Global for LocalizationChanged {}

pub(crate) trait LocalizedPlaceholder: Sized + 'static {
    fn update_placeholder(
        &mut self,
        value: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    );
}

macro_rules! localized_placeholder {
    ($state:ty) => {
        impl LocalizedPlaceholder for $state {
            fn update_placeholder(
                &mut self,
                value: String,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) {
                self.set_placeholder(value, window, cx);
            }
        }
    };
}
localized_placeholder!(gpui_component::input::InputState);
localized_placeholder!(gpui_component::input::TextareaState);

/// Bind cached control wording to the shared language/override revision without
/// replacing the control, its draft, selection, undo history or focus.
pub(crate) fn input_placeholder<T: LocalizedPlaceholder>(
    window: &gpui::Window,
    cx: &mut gpui::Context<T>,
    key: &'static str,
) -> String {
    input_placeholder_with(window, cx, move || text(key))
}

pub(crate) fn input_placeholder_with<T: LocalizedPlaceholder>(
    window: &gpui::Window,
    cx: &mut gpui::Context<T>,
    wording: impl Fn() -> String + 'static,
) -> String {
    let initial = wording();
    cx.observe_global_in::<LocalizationChanged>(window, move |input, window, cx| {
        input.update_placeholder(wording(), window, cx);
    })
    .detach();
    initial
}

/// Bind a fixed, ordered choice list while retaining its selected business index.
pub(crate) fn select_labels(
    window: &gpui::Window,
    cx: &mut gpui::Context<gpui_component::select::SelectState<Vec<String>>>,
    labels: impl Fn() -> Vec<String> + 'static,
) -> Vec<String> {
    let initial = labels();
    let mut previous = initial.clone();
    cx.observe_global_in::<LocalizationChanged>(window, move |select, window, cx| {
        let selected = select
            .selected_value()
            .and_then(|value| previous.iter().position(|label| label == value));
        let next = labels();
        select.set_items(next.clone(), window, cx);
        if let Some(value) = selected.and_then(|index| next.get(index)) {
            select.set_selected_value(value, window, cx);
        } else {
            select.set_selected_index(None, window, cx);
        }
        previous = next;
    })
    .detach();
    initial
}

/// Wording source for one OS window's title, kept so a language switch can
/// re-title windows opened before it.
type WindowTitleSource = std::rc::Rc<dyn Fn() -> String>;

thread_local! {
    static WINDOW_TITLES: std::cell::RefCell<std::collections::HashMap<gpui::WindowId, WindowTitleSource>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Re-title `window` from `title` whenever the language or overrides change.
/// The OS title is set once at open; this keeps it in step afterwards.
pub(crate) fn register_window_title(
    window: gpui::AnyWindowHandle,
    title: impl Fn() -> String + 'static,
) {
    WINDOW_TITLES.with(|titles| {
        titles
            .borrow_mut()
            .insert(window.window_id(), std::rc::Rc::new(title));
    });
}

/// Register a window whose title is the catalog text of `key`.
pub(crate) fn register_window_title_key(window: gpui::AnyWindowHandle, key: &'static str) {
    register_window_title(window, move || text(key));
}

/// The current-language title of a registered window.
pub(crate) fn window_title(window: gpui::WindowId) -> Option<String> {
    let source = WINDOW_TITLES.with(|titles| titles.borrow().get(&window).cloned())?;
    Some(source())
}

/// Forget windows that have closed.
pub(crate) fn retain_window_titles(open: &[gpui::AnyWindowHandle]) {
    let open: std::collections::HashSet<gpui::WindowId> =
        open.iter().map(|window| window.window_id()).collect();
    WINDOW_TITLES.with(|titles| titles.borrow_mut().retain(|id, _| open.contains(id)));
}

/// Host (website translation manager) language switch. Like Settings, this
/// replaces language and overrides together: overrides written for the old
/// language are dropped, and the manager re-pushes the new language's set.
pub fn apply_host_language(language_code: &str, cx: &mut gpui::App) -> Result<(), String> {
    activate_host_language(language_code)?;
    refresh(cx);
    Ok(())
}

fn activate_host_language(language_code: &str) -> Result<(), String> {
    let language = Language::from_code(language_code)
        .ok_or_else(|| format!("Unsupported language '{language_code}'"))?;
    activate(language, None)
}

/// Host override push. The manager sends its whole server catalog, which can
/// be newer or older than this build, so unknown keys and entries whose
/// placeholders differ from English are skipped (and returned for the host's
/// acknowledgement) instead of rejecting the live preview. Malformed JSON is
/// still an error that leaves the current overrides in place. Custom files
/// chosen in Settings stay strict.
pub fn apply_host_overrides(
    overrides_json: &str,
    cx: &mut gpui::App,
) -> Result<Vec<String>, String> {
    let skipped = activate_host_overrides(overrides_json)?;
    refresh(cx);
    Ok(skipped)
}

fn activate_host_overrides(overrides_json: &str) -> Result<Vec<String>, String> {
    let skipped = sqlly_i18n::set_overrides_json_lenient(overrides_json)?;
    log_skipped_overrides("host translation push", &skipped);
    Ok(skipped.into_iter().map(|skip| skip.key).collect())
}

/// How many skipped keys a log line or warning names before summarizing.
const SKIPPED_KEYS_SHOWN: usize = 10;

/// The first [`SKIPPED_KEYS_SHOWN`] keys, comma separated, with "…" when more
/// were skipped.
fn skipped_key_list<'a>(keys: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = keys.len();
    let mut list = keys.take(SKIPPED_KEYS_SHOWN).collect::<Vec<_>>().join(", ");
    if total > SKIPPED_KEYS_SHOWN {
        list.push_str(", …");
    }
    list
}

/// One summary line for a lenient override load, however many keys a skewed
/// catalog skips (a whole-catalog push can skip hundreds).
fn log_skipped_overrides(source: &str, skipped: &[sqlly_i18n::SkippedOverride]) {
    let Some(first) = skipped.first() else {
        return;
    };
    let keys = skipped_key_list(skipped.iter().map(|skip| skip.key.as_str()));
    tracing::warn!(
        source,
        count = skipped.len(),
        %keys,
        first_reason = %first.reason,
        "skipped translation overrides"
    );
}

pub fn clear_host_overrides(cx: &mut gpui::App) -> Result<(), String> {
    sqlly_i18n::clear_overrides();
    refresh(cx);
    Ok(())
}

pub fn text(key: &str) -> String {
    sqlly_i18n::text(key)
}

/// Format a catalog message with named `{placeholder}` arguments. Never
/// renders an empty or half-built string: when the active language (or an
/// override) cannot be formatted with these arguments, the compiled English
/// message is used, and if even that fails the key itself is shown. Both
/// failures are logged; the second is a programming error (the call site's
/// argument names do not match the English placeholders), which
/// `tests/translation_coverage.rs` rejects statically.
pub fn format<T: std::fmt::Display>(key: &str, arguments: &[(&str, T)]) -> String {
    // Values are written straight into the message; no per-argument Strings.
    let error = match sqlly_i18n::format_with(key, arguments) {
        Ok(text) => return text,
        Err(error) => error,
    };
    match sqlly_i18n::format_compiled_with(Language::English, key, arguments) {
        Ok(english) => {
            tracing::warn!(key, %error, "translation could not be formatted; showing English");
            english
        }
        Err(english_error) => {
            tracing::warn!(key, %english_error, "catalog message could not be formatted; showing its key");
            debug_assert!(
                false,
                "catalog message '{key}' could not be formatted: {english_error}"
            );
            key.to_owned()
        }
    }
}

/// Run `resolve` over borrowed `(name, value)` pairs without a heap
/// allocation for the common case of at most eight arguments.
fn with_argument_slice<'a, R>(
    mut arguments: impl Iterator<Item = (&'a str, &'a str)>,
    resolve: impl FnOnce(&[(&'a str, &'a str)]) -> R,
) -> R {
    const INLINE: usize = 8;
    let mut inline = [("", ""); INLINE];
    let mut count = 0;
    while let Some(argument) = arguments.next() {
        if count == INLINE {
            let mut spilled = inline.to_vec();
            spilled.push(argument);
            spilled.extend(arguments);
            return resolve(&spilled);
        }
        inline[count] = argument;
        count += 1;
    }
    resolve(&inline[..count])
}

/// Format a known key in the active language, or `None` when the key is not in
/// the catalog or the arguments do not fit it. One catalog lookup.
fn format_known<'a>(
    key: &str,
    arguments: impl Iterator<Item = (&'a str, &'a str)>,
) -> Option<String> {
    with_argument_slice(arguments, |arguments| {
        sqlly_i18n::format_if_known(key, arguments).and_then(Result::ok)
    })
}

pub fn message(descriptor: impl std::borrow::Borrow<sqlly_core::DisplayMessage>) -> String {
    let descriptor = descriptor.borrow();
    descriptor
        .key()
        .and_then(|key| format_known(key, descriptor.arguments()))
        .unwrap_or_else(|| descriptor.fallback().to_owned())
}

/// Render a persisted catalog message (key + arguments saved with a record)
/// in the active language. Returns `fallback`, the English text stored beside
/// it, when the key is no longer in the catalog or its placeholders changed.
pub fn stored_message(stored: &sqlly_core::StoredMessage, fallback: &str) -> String {
    let arguments = stored
        .arguments
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()));
    format_known(&stored.key, arguments).unwrap_or_else(|| fallback.to_owned())
}

/// [`stored_message`] for an optional key: the English `fallback` as is when
/// the record carries no catalog message (raw text, or an older peer).
pub fn stored_message_or(stored: Option<&sqlly_core::StoredMessage>, fallback: String) -> String {
    match stored {
        Some(stored) => stored_message(stored, &fallback),
        None => fallback,
    }
}

/// A keyed message built where the UI language is not visible: on the tokio
/// runtime, the blocking pool, or a background executor (translation state is
/// per thread). Carries the key and arguments plus an English fallback; the UI
/// thread resolves it with [`message`]. Safe on any thread.
pub fn deferred<T: std::fmt::Display>(
    key: &'static str,
    arguments: &[(&str, T)],
) -> sqlly_core::DisplayMessage {
    let fallback = sqlly_i18n::format_compiled_with(Language::English, key, arguments)
        .unwrap_or_else(|_| key.to_owned());
    arguments.iter().fold(
        sqlly_core::DisplayMessage::new(key, fallback),
        |message, (name, value)| message.with_arg(*name, value.to_string()),
    )
}

/// [`deferred`] for a message without arguments.
pub fn deferred_text(key: &'static str) -> sqlly_core::DisplayMessage {
    deferred::<&str>(key, &[])
}

/// Activate a language and its selected-language overrides as one operation.
/// Invalid JSON leaves the active language, overrides, and component locale intact.
pub fn activate(language: Language, overrides_json: Option<&str>) -> Result<(), String> {
    sqlly_i18n::activate(language, overrides_json)?;
    sync_component_locale();
    Ok(())
}

/// The language and overrides active right now, to put back with
/// [`restore`] if a multi-step change (activate, then save) fails part-way.
pub(crate) fn snapshot() -> sqlly_i18n::Snapshot {
    sqlly_i18n::snapshot()
}

/// Restore a [`snapshot`] (language, overrides, and the stock-component
/// locale). The caller refreshes open windows.
pub(crate) fn restore(snapshot: sqlly_i18n::Snapshot) {
    sqlly_i18n::restore(snapshot);
    sync_component_locale();
}

/// The language the UI is showing now. On the browser build the host page can
/// switch it, so this, not the saved preference, is what Settings starts from.
pub(crate) fn current_language() -> Language {
    sqlly_i18n::current_language()
}

/// What went wrong loading the saved custom translation file at startup, kept
/// so the Settings Language pane can show it (startup itself only logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CustomFileIssue {
    /// The file loaded, but these entries were left out.
    Skipped(Vec<String>),
    /// The file could not be read or parsed; compiled text is shown.
    Failed(String),
}

static STARTUP_CUSTOM_FILE_ISSUE: std::sync::Mutex<Option<CustomFileIssue>> =
    std::sync::Mutex::new(None);

fn set_startup_custom_file_issue(issue: Option<CustomFileIssue>) {
    *STARTUP_CUSTOM_FILE_ISSUE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = issue;
}

/// Forget the startup issue once Settings applies a file strictly.
pub(crate) fn clear_startup_custom_file_issue() {
    set_startup_custom_file_issue(None);
}

/// The startup issue with the saved custom file, worded in the active language.
pub(crate) fn startup_custom_file_warning() -> Option<String> {
    let issue = STARTUP_CUSTOM_FILE_ISSUE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()?;
    Some(match issue {
        CustomFileIssue::Failed(error) => format(
            "settings.appearance.language.startup_failed",
            &[("error", error)],
        ),
        CustomFileIssue::Skipped(keys) => format(
            "settings.appearance.language.startup_skipped",
            &[
                ("count", keys.len().to_string()),
                ("keys", skipped_key_list(keys.iter().map(String::as_str))),
            ],
        ),
    })
}

/// Activate `language` with a saved custom file's contents, keeping every
/// valid entry: a key renamed between versions must not discard the rest of
/// the file at startup. Settings → Apply stays strict.
fn activate_saved_overrides(language: Language, json: Option<&str>) -> Option<CustomFileIssue> {
    let Some(json) = json else {
        let _ = activate(language, None);
        return None;
    };
    match sqlly_i18n::activate_lenient(language, json) {
        Ok(skipped) => {
            sync_component_locale();
            log_skipped_overrides("saved custom translation file", &skipped);
            (!skipped.is_empty()).then(|| {
                CustomFileIssue::Skipped(skipped.into_iter().map(|skip| skip.key).collect())
            })
        }
        Err(error) => {
            tracing::warn!(%error, "invalid custom translations; using compiled defaults");
            let _ = activate(language, None);
            Some(CustomFileIssue::Failed(error))
        }
    }
}

/// Load persisted language settings before menus or windows are built. A bad
/// optional override file must never prevent the application from starting.
pub fn activate_saved() {
    initialize_component_translations();
    let prefs = crate::preferences::AppearancePreferences::load();
    let language = Language::from_code(&prefs.language).unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let (overrides, read_issue) = match prefs.custom_translation_file.as_ref() {
        None => (None, None),
        Some(path) => match std::fs::read_to_string(path) {
            Ok(json) => (Some(json), None),
            Err(error) => {
                tracing::warn!(%error, path, "could not load custom translations; using compiled defaults");
                (None, Some(CustomFileIssue::Failed(error.to_string())))
            }
        },
    };
    #[cfg(target_arch = "wasm32")]
    let (overrides, read_issue): (Option<String>, Option<CustomFileIssue>) = (None, None);
    let issue = activate_saved_overrides(language, overrides.as_deref());
    set_startup_custom_file_issue(read_issue.or(issue));
}

/// Serializes tests that switch the UI language. The catalog state is per
/// thread, but the stock-component locale is process-wide, so a test that
/// switches language without this lock can flip another test's components.
#[cfg(test)]
static TEST_LANGUAGE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`TEST_LANGUAGE`] and restores English (no overrides) on drop, even
/// when the test panics. Tests may call [`activate`] freely while holding it.
#[cfg(test)]
pub(crate) struct TestLanguageGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestLanguageGuard {
    fn drop(&mut self) {
        let _ = activate(Language::English, None);
    }
}

/// Lock the test language and activate `language` without overrides.
#[cfg(test)]
pub(crate) fn test_language(language: Language) -> TestLanguageGuard {
    test_language_with(language, None)
}

/// Lock the test language and activate `language` with `overrides_json`.
#[cfg(test)]
pub(crate) fn test_language_with(
    language: Language,
    overrides_json: Option<&str>,
) -> TestLanguageGuard {
    let lock = TEST_LANGUAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    activate(language, overrides_json).expect("valid test translation overrides");
    TestLanguageGuard { _lock: lock }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language_guard() -> TestLanguageGuard {
        test_language(Language::English)
    }

    fn component_text(key: &str) -> String {
        gpui_component::_rust_i18n_translate(&gpui_component::locale(), key).into_owned()
    }

    #[test]
    fn stock_components_default_to_english_before_language_activation() {
        let _guard = language_guard();
        sqlly_i18n::set_language(Language::English);
        initialize_component_translations();
        assert_eq!(&*gpui_component::locale(), "en");
        assert_eq!(component_text("Input.Copy"), "Copy");
    }

    #[test]
    fn invalid_override_does_not_partially_change_language() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        assert!(activate(Language::Spanish, Some("not json")).is_err());
        assert_eq!(sqlly_i18n::current_language(), Language::English);
        assert_eq!(&*gpui_component::locale(), "en");
        assert_eq!(text("settings.appearance.language.title"), "Language");
    }

    #[test]
    fn clearing_the_file_restores_compiled_values() {
        let _guard = language_guard();
        activate(Language::Spanish, Some(r#"{"settings":{"appearance":{"language":{"title":{"value":"Idioma personalizado","location":"Settings → Appearance → Language: test override for the language pane title."}}}}}"#)).unwrap();
        assert_eq!(
            text("settings.appearance.language.title"),
            "Idioma personalizado"
        );
        activate(Language::Spanish, None).unwrap();
        assert_eq!(text("settings.appearance.language.title"), "Idioma");
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn every_stock_component_key_maps_to_a_direct_catalog_entry() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        assert!(COMPONENT_KEYS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (component_key, catalog_key) in COMPONENT_KEYS {
            assert!(sqlly_i18n::contains_key(catalog_key), "{catalog_key}");
            assert_ne!(component_text(component_key), *component_key);
        }
        assert_eq!(component_text("Input.Cut"), "Cut");
        assert_eq!(
            component_text("Carousel.slide"),
            "Slide %{current} of %{total}"
        );
        assert!(
            SqllyComponentBackend
                .translate("en", "gpui_component.Input.Unknown")
                .is_none()
        );
    }

    #[test]
    fn stock_components_use_spanish_and_host_overrides() {
        let _guard = language_guard();
        let overrides = r#"{"components":{"input":{"cut":{"value":"Corte personalizado","location":"Stock components → Input → Context menu: test override for the Cut command."}}}}"#;
        activate(Language::Spanish, Some(overrides)).unwrap();
        assert_eq!(&*gpui_component::locale(), "es");
        assert_eq!(component_text("Input.Cut"), "Corte personalizado");
        assert_eq!(component_text("Dialog.cancel"), "Cancelar");
        assert_eq!(
            component_text("Diff.HiddenLines"),
            "%{count} líneas no incluidas en el parche"
        );
        sqlly_i18n::clear_overrides();
        assert_eq!(component_text("Input.Cut"), "Cortar");
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn display_messages_resolve_keyed_arguments_and_preserve_raw_text() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        let descriptor = sqlly_core::DisplayMessage::new(
            "datatable.grid.search.position",
            "{current} of {total}",
        )
        .with_arg("current", "3")
        .with_arg("total", "12");
        assert_eq!(message(&descriptor), "3 de 12");
        assert_eq!(
            message(sqlly_core::DisplayMessage::raw("Server supplied {text}")),
            "Server supplied {text}"
        );
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn display_messages_use_fallback_for_unknown_keys_and_format_failures() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        assert_eq!(
            message(sqlly_core::DisplayMessage::new(
                "services.missing.message",
                "Fallback detail",
            )),
            "Fallback detail"
        );
        assert_eq!(
            message(sqlly_core::DisplayMessage::new(
                "datatable.grid.search.position",
                "Match 4 of 9",
            )),
            "Match 4 of 9"
        );
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn format_accepts_display_values_and_existing_str_slices() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        assert_eq!(
            format(
                "datatable.grid.search.position",
                &[("current", String::from("2")), ("total", String::from("9"))],
            ),
            "2 of 9"
        );
        assert_eq!(
            format(
                "datatable.grid.search.position",
                &[("current", 3), ("total", 12)],
            ),
            "3 of 12"
        );
        assert_eq!(
            format(
                "datatable.grid.drill_filter.banner",
                &[("filter_label", std::io::Error::other("permission denied"),)],
            ),
            "Filtered from pivot: permission denied"
        );
        let existing = [("current", "4"), ("total", "10")];
        assert_eq!(
            format("datatable.grid.search.position", &existing),
            "4 of 10"
        );
        let no_arguments: &[(&str, &str)] = &[];
        assert_eq!(
            format("datatable.grid.search.no_matches", no_arguments),
            "No matches"
        );
    }

    #[test]
    fn format_preserves_named_placeholder_validation_for_display_values() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        assert!(
            sqlly_i18n::format_with("datatable.grid.search.position", &[("current", 3)]).is_err()
        );
        assert!(
            sqlly_i18n::format_with(
                "datatable.grid.search.position",
                &[("current", 3), ("total", 12), ("unused", 9)],
            )
            .is_err()
        );
        assert!(sqlly_i18n::format_with("missing.translation.key", &[("count", 3)]).is_err());
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn format_shows_the_key_when_arguments_do_not_match_english() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        assert_eq!(
            format("datatable.grid.search.position", &[("current", 3)]),
            "datatable.grid.search.position"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "could not be formatted")]
    fn format_with_mismatched_arguments_is_a_debug_assertion() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        let _ = format("datatable.grid.search.position", &[("current", 3)]);
    }

    #[test]
    fn datatable_translator_follows_language_changes_and_formats_messages() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        assert_eq!(
            sqlly_datatable::localization::text("datatable.grid.search.no_matches"),
            "Sin coincidencias"
        );
        assert_eq!(
            sqlly_datatable::localization::format(
                "datatable.grid.search.position",
                &[("current", "2"), ("total", "10")],
            ),
            "2 de 10"
        );
        assert_eq!(
            sqlly_datatable::localization::text("datatable.unknown"),
            "datatable.unknown"
        );
        activate(Language::English, None).unwrap();
        assert_eq!(
            sqlly_datatable::localization::text("datatable.grid.search.no_matches"),
            "No matches"
        );
        assert_eq!(
            sqlly_datatable::localization::format(
                "datatable.grid.search.position",
                &[("current", "2"), ("total", "10")],
            ),
            "2 of 10"
        );
    }

    #[test]
    fn host_language_switch_drops_the_previous_languages_overrides() {
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        let overrides = r#"{"settings":{"appearance":{"language":{"title":{"value":"English host title","location":"Settings → Appearance → Language: host override for the language pane title."}}}}}"#;
        assert!(activate_host_overrides(overrides).unwrap().is_empty());
        assert_eq!(
            text("settings.appearance.language.title"),
            "English host title"
        );
        activate_host_language("es").unwrap();
        assert_eq!(&*gpui_component::locale(), "es");
        assert_eq!(text("settings.appearance.language.title"), "Idioma");
        assert!(activate_host_language("unsupported").is_err());
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
        activate(Language::English, None).unwrap();
    }

    #[gpui::test]
    fn cached_inputs_refresh_in_multiple_windows_without_losing_drafts(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::{AppContext, Render};
        struct Inputs(
            gpui::Entity<gpui_component::input::InputState>,
            gpui::Entity<gpui_component::select::SelectState<Vec<String>>>,
        );
        impl Render for Inputs {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui_component::input::Input::new(&self.0)
            }
        }
        let _guard = language_guard();
        activate(Language::English, None).unwrap();
        cx.update(gpui_component::init);
        let mut inputs = Vec::new();
        let mut selects = Vec::new();
        for _ in 0..2 {
            let (root, window_cx) = cx.add_window_view(|window, cx| {
                let view = cx.new(|cx| {
                    Inputs(
                        cx.new(|cx| {
                            gpui_component::input::InputState::new(window, cx)
                                .placeholder(input_placeholder(
                                    window,
                                    cx,
                                    "settings.init.placeholder.search",
                                ))
                                .default_value("unsaved draft")
                        }),
                        cx.new(|cx| {
                            gpui_component::select::SelectState::new(
                                select_labels(window, cx, || {
                                    vec![
                                        text("explorer.er_diagram.layout.grid"),
                                        text("explorer.er_diagram.layout.layered"),
                                    ]
                                }),
                                Some(gpui_component::IndexPath::new(1)),
                                window,
                                cx,
                            )
                        }),
                    )
                });
                gpui_component::Root::new(view, window, cx)
            });
            let input = root.read_with(window_cx, |root, cx| {
                root.view()
                    .clone()
                    .downcast::<Inputs>()
                    .unwrap()
                    .read(cx)
                    .0
                    .clone()
            });
            inputs.push(input);
            selects.push(root.read_with(window_cx, |root, cx| {
                root.view()
                    .clone()
                    .downcast::<Inputs>()
                    .unwrap()
                    .read(cx)
                    .1
                    .clone()
            }));
        }
        cx.update(|cx| apply_host_language("es", cx).unwrap());
        cx.run_until_parked();
        cx.update(|cx| {
            for select in &selects {
                assert_eq!(
                    select.read(cx).selected_index(cx),
                    Some(gpui_component::IndexPath::new(1))
                );
                assert_eq!(
                    select.read(cx).selected_value(),
                    Some(&text("explorer.er_diagram.layout.layered"))
                );
            }
            for input in &inputs {
                assert_eq!(
                    input.read(cx).presentation().placeholder().as_ref(),
                    text("settings.init.placeholder.search")
                );
                assert_eq!(input.read(cx).value().as_ref(), "unsaved draft");
            }
            apply_host_language("en", cx).unwrap();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            for input in &inputs {
                assert_eq!(
                    input.read(cx).presentation().placeholder().as_ref(),
                    text("settings.init.placeholder.search")
                );
            }
        });
    }

    #[test]
    fn host_override_push_skips_keys_this_build_does_not_know() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        let overrides = r#"{"settings":{"appearance":{"language":{"title":{"value":"Idioma del sitio","location":"Settings → Appearance → Language: host override for the language pane title."}}},"newer":{"screen":{"label":{"value":"Nuevo","location":"Newer → Screen: label that only a newer server catalog contains."}}}}}"#;
        assert_eq!(
            activate_host_overrides(overrides).unwrap(),
            ["settings.newer.screen.label"]
        );
        assert_eq!(
            text("settings.appearance.language.title"),
            "Idioma del sitio"
        );
        assert!(activate_host_overrides("not json").is_err());
        assert_eq!(
            text("settings.appearance.language.title"),
            "Idioma del sitio"
        );
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn saved_custom_file_keeps_valid_entries_when_one_key_was_renamed() {
        let _guard = language_guard();
        let json = r#"{"settings":{"appearance":{"language":{"title":{"value":"Mi idioma","location":"Settings → Appearance → Language: test override for the language pane title."}}}},"retired":{"screen":{"label":{"value":"Viejo","location":"Retired → Screen: label whose key a newer build renamed away."}}}}"#;
        let issue = activate_saved_overrides(Language::Spanish, Some(json));
        assert_eq!(
            issue,
            Some(CustomFileIssue::Skipped(vec![
                "retired.screen.label".into()
            ]))
        );
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
        assert_eq!(&*gpui_component::locale(), "es");
        assert_eq!(text("settings.appearance.language.title"), "Mi idioma");

        let issue = activate_saved_overrides(Language::Spanish, Some("not json"));
        assert!(
            matches!(issue, Some(CustomFileIssue::Failed(_))),
            "{issue:?}"
        );
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
        assert_eq!(text("settings.appearance.language.title"), "Idioma");
        assert_eq!(activate_saved_overrides(Language::English, None), None);
    }

    #[test]
    fn startup_custom_file_warning_names_skipped_keys_in_the_active_language() {
        let _guard = language_guard();
        let keys = (0..12)
            .map(|index| format!("retired.key_{index}"))
            .collect::<Vec<_>>();
        set_startup_custom_file_issue(Some(CustomFileIssue::Skipped(keys)));
        let warning = startup_custom_file_warning().unwrap();
        assert!(warning.starts_with("12 entries"), "{warning}");
        assert!(warning.contains("retired.key_9, …"), "{warning}");
        assert!(!warning.contains("retired.key_10"), "{warning}");
        activate(Language::Spanish, None).unwrap();
        set_startup_custom_file_issue(Some(CustomFileIssue::Failed("EOF".into())));
        let warning = startup_custom_file_warning().unwrap();
        assert!(warning.starts_with("No se pudo cargar"), "{warning}");
        assert!(warning.ends_with("EOF"), "{warning}");
        clear_startup_custom_file_issue();
        assert_eq!(startup_custom_file_warning(), None);
    }

    #[test]
    fn restoring_a_snapshot_puts_back_language_overrides_and_component_locale() {
        let _guard = language_guard();
        let overrides = r#"{"components":{"input":{"cut":{"value":"Corte del anfitrión","location":"Stock components → Input → Context menu: test override for the Cut command."}}}}"#;
        activate(Language::Spanish, Some(overrides)).unwrap();
        let before = snapshot();
        activate(Language::English, None).unwrap();
        assert_eq!(&*gpui_component::locale(), "en");
        restore(before);
        assert_eq!(current_language(), Language::Spanish);
        assert_eq!(&*gpui_component::locale(), "es");
        assert_eq!(component_text("Input.Cut"), "Corte del anfitrión");
    }

    #[test]
    fn message_formats_known_keys_with_many_arguments_without_spilling_wrong() {
        let _guard = language_guard();
        let descriptor =
            sqlly_core::DisplayMessage::new("query.execution_plan.node_properties", "fallback")
                .with_arg("node_id", "7")
                .with_arg("physical_operation", "Hash Match")
                .with_arg("logical_operation", "Inner Join")
                .with_arg("estimated_cost", "0.42")
                .with_arg("cost_percent", "18")
                .with_arg("estimated_rows", "1200")
                .with_arg("actual_rows", "1180")
                .with_arg("warnings", "none");
        let rendered = message(&descriptor);
        assert_ne!(rendered, "fallback");
        assert!(rendered.contains("Hash Match"), "{rendered}");
        let spilled = descriptor.clone().with_arg("extra", "9");
        assert_eq!(message(&spilled), "fallback");
    }

    #[test]
    fn deferred_messages_from_worker_threads_resolve_in_the_ui_language() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        let built = std::thread::spawn(|| {
            deferred(
                "datatable.grid.search.position",
                &[("current", 2), ("total", 9)],
            )
        })
        .join()
        .unwrap();
        assert_eq!(built.fallback(), "2 of 9");
        assert_eq!(message(&built), "2 de 9");
        assert_eq!(
            deferred_text("datatable.grid.search.no_matches").fallback(),
            "No matches"
        );
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn stock_components_keep_the_active_language_on_background_threads() {
        let _guard = language_guard();
        activate(Language::Spanish, None).unwrap();
        std::thread::spawn(|| {
            assert_eq!(sqlly_i18n::current_language(), Language::English);
            assert_eq!(component_text("Input.Cut"), "Cortar");
            assert_eq!(component_text("Calendar.month.January"), "enero");
        })
        .join()
        .unwrap();
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
        activate(Language::English, None).unwrap();
    }

    #[test]
    fn not_editable_reasons_from_the_engine_protocol_render_in_spanish() {
        let _guard = language_guard();
        // A report as a current engine or bridge sends it over the wire.
        let json = r#"{"NotEditable":{"reason":"key column [Id] is NULL in this row; the row cannot be identified","message":{"key":"services.row_editing.not_editable.key_column_null","arguments":[["column_name","[Id]"]]}}}"#;
        let outcome: sqlly_services::row_editing::RowEditOpenOutcome =
            serde_json::from_str(json).unwrap();
        let sqlly_services::row_editing::RowEditOpenOutcome::NotEditable { reason, message } =
            outcome
        else {
            panic!("expected NotEditable");
        };
        activate(Language::Spanish, None).unwrap();
        assert_eq!(
            stored_message_or(message.as_ref(), reason.clone()),
            "la columna clave [Id] es NULL en esta fila; no se puede identificar la fila"
        );
        // An older peer sends only the English reason: shown as is.
        assert_eq!(stored_message_or(None, reason.clone()), reason);
        activate(Language::English, None).unwrap();
        assert_eq!(stored_message_or(message.as_ref(), reason.clone()), reason);
    }

    #[test]
    fn every_not_editable_reason_key_is_in_the_catalog_with_its_english_fallback() {
        let source = include_str!("../../../crates/sqlly-services/src/row_editing/not_editable.rs");
        let keys: Vec<&str> = source
            .split('"')
            .filter(|piece| {
                piece.starts_with("services.row_editing.not_editable.")
                    && piece.len() > "services.row_editing.not_editable.".len()
            })
            .collect();
        assert!(keys.len() >= 20, "{keys:?}");
        for key in keys {
            assert!(sqlly_i18n::contains_key(key), "{key} missing from catalog");
            assert!(
                sqlly_i18n::compiled_value(Language::Spanish, key).is_some(),
                "{key} missing Spanish"
            );
        }
        let policy = [
            "services.row_editing.policy.select_only",
            "services.row_editing.policy.auto_rollback",
        ];
        for key in policy {
            assert!(sqlly_i18n::contains_key(key), "{key}");
        }
    }
}
