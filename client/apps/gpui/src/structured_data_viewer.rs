//! GPUI window for viewing pretty-printed JSON/XML cell values with syntax
//! highlighting. Port of `StructuredDataViewerController.swift`.
//!
//! The pure detection, pretty-printing, syntax classification, and tree
//! model live in `structured_data.rs`. This module only handles the GPUI
//! rendering: a Raw mode (scrollable, syntax-highlighted text) and a Tree
//! mode (collapsible rows with search and per-node copy affordances).

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, IntoElement, ObjectFit, Render, ScrollHandle,
    SharedString, Styled, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    img, prelude::*, px, size,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use sqlly_core::CellValue;
use sqlly_core::spatial::Geometry;

use crate::spatial_canvas::spatial_canvas;
use crate::structured_data::{
    ContentPreview, FORMAT_PICKER_MAX_BYTES, FormatGroup, RenderedPayload, StructuredDataKind,
    StructuredNode, SyntaxSpan, SyntaxToken, ValueFormat, build_tree, classify, classify_cell,
    detect, flatten_visible, hex_dump, pretty_print, render_payload, search_rows,
};
use crate::theme::Theme;
use crate::tooltip::AppTooltip as _;

/// Maximum number of flattened tree rows. Rendering is windowed to the
/// viewport; anything beyond this cap gets a truncation notice.
const MAX_TREE_ROWS: usize = 5000;

/// Inclusive-exclusive window of flattened rows to paint.
///
/// Copied from `schema_tree.rs` (the `scroll_y` / `first` / `count` / overscan
/// formula). Unlike schema tree, a zero viewport does **not** fall back to the
/// full list — that would rebuild up to [`MAX_TREE_ROWS`] elements on the first
/// frame.
fn tree_window_bounds(
    viewport_h: f32,
    offset_y: f32,
    row_h: f32,
    row_count: usize,
) -> (usize, usize) {
    if row_count == 0 || row_h <= 0.0 {
        return (0, 0);
    }
    let scroll_y = (-offset_y).max(0.0);
    let first = (scroll_y / row_h).floor() as usize;
    let count = (viewport_h / row_h).ceil() as usize + 6;
    let start = first.saturating_sub(2).min(row_count);
    let end = start.saturating_add(count).min(row_count);
    (start, end)
}

/// Cached flatten of the JSON/XML tree, keyed by collapsed paths + search.
struct FlattenCache {
    collapsed: HashSet<String>,
    search: String,
    rows: Vec<RowData>,
    truncated: usize,
    populated: bool,
}

impl FlattenCache {
    fn new() -> Self {
        Self {
            collapsed: HashSet::new(),
            search: String::new(),
            rows: Vec::new(),
            truncated: 0,
            populated: false,
        }
    }

    fn clear(&mut self) {
        self.populated = false;
        self.rows.clear();
        self.truncated = 0;
    }

    fn refresh(
        &mut self,
        tree: &StructuredNode,
        collapsed: &HashSet<String>,
        search: &str,
    ) -> bool {
        if self.populated && self.collapsed == *collapsed && self.search == search {
            return true;
        }
        let (rows, truncated) = collect_visible_rows(tree, collapsed, search);
        self.collapsed = collapsed.clone();
        self.search = search.to_string();
        self.rows = rows;
        self.truncated = truncated;
        self.populated = true;
        false
    }
}

/// Owned snapshots for the current collapse/search state, capped at
/// [`MAX_TREE_ROWS`].
fn collect_visible_rows(
    tree: &StructuredNode,
    collapsed: &HashSet<String>,
    search: &str,
) -> (Vec<RowData>, usize) {
    let searching = !search.trim().is_empty();
    let flat = if searching {
        search_rows(tree, search)
    } else {
        flatten_visible(tree, collapsed)
    };
    let total = flat.len();
    let truncated = total.saturating_sub(MAX_TREE_ROWS);
    let rows = flat
        .into_iter()
        .take(MAX_TREE_ROWS)
        .map(|r| RowData {
            depth: r.depth,
            label: r.node.label.clone(),
            preview: r.node.value_preview.clone(),
            path: r.node.path.clone(),
            is_branch: r.node.is_branch(),
            child_count: r.node.children.len(),
            is_collapsed: !searching && collapsed.contains(&r.node.path),
            is_marker: r.node.is_truncation_marker(),
        })
        .collect();
    (rows, truncated)
}

/// Which body the viewer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerMode {
    Raw,
    Tree,
    Sketch,
    /// Geometry on a pannable map (see `map_view`).
    Map,
    /// Rendered Markdown.
    Preview,
}

/// Hands re-compressed bytes to whoever opened the viewer (the results grid
/// stages them as a cell edit). Absent, the viewer offers no Edit button.
pub type SaveBytes = std::rc::Rc<dyn Fn(Vec<u8>, &mut App)>;

/// The open edit-and-re-compress editor.
struct RecompressEditor {
    framing: crate::value_recompress::Framing,
    input: Entity<TextareaState>,
    error: Option<String>,
}

/// Everything Auto mode computed at construction, so switching a format on
/// and back off restores the exact auto-detected view.
#[derive(Clone)]
struct AutoSnapshot {
    text: String,
    kind: StructuredDataKind,
    spans: Vec<SyntaxSpan>,
    tree: Option<StructuredNode>,
    mode: ViewerMode,
    /// What Auto's sniff found and decoded ("gzip → JSON (…)"), if
    /// anything; `None` for the plain detected view.
    note: Option<String>,
}

impl AutoSnapshot {
    /// The view a rendered payload makes: JSON and XML get their
    /// highlighter and tree, anything else is raw text (a hex dump or
    /// plain text).
    fn from_rendered(rendered: &crate::structured_data::RenderedPayload) -> Self {
        let kind = if rendered.as_json {
            StructuredDataKind::Json
        } else if rendered.as_xml {
            StructuredDataKind::Xml
        } else if rendered.as_yaml {
            StructuredDataKind::Yaml
        } else if rendered.as_markdown {
            StructuredDataKind::Markdown
        } else {
            StructuredDataKind::Hex
        };
        let structured =
            rendered.as_json || rendered.as_xml || rendered.as_yaml || rendered.as_markdown;
        let spans = if structured {
            classify(&rendered.text, kind)
        } else {
            Vec::new()
        };
        let tree = if structured {
            build_tree(&rendered.text, kind)
        } else {
            None
        };
        let mode = initial_mode(kind, tree.is_some());
        Self {
            text: rendered.text.clone(),
            kind,
            spans,
            tree,
            mode,
            note: rendered.note.clone(),
        }
    }
}

/// The body a fresh view opens on: the tree when there is one, the
/// rendered preview for Markdown, raw text otherwise.
fn initial_mode(kind: StructuredDataKind, has_tree: bool) -> ViewerMode {
    if has_tree {
        ViewerMode::Tree
    } else if kind == StructuredDataKind::Markdown {
        ViewerMode::Preview
    } else {
        ViewerMode::Raw
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageZoom {
    Fit,
    Actual,
}

/// The viewer entity rendered inside the window.
pub struct StructuredDataViewer {
    /// Pretty-printed text ready for display.
    text: String,
    /// Detected kind (JSON or XML).
    kind: StructuredDataKind,
    /// Column name from the source result set (for the title bar).
    column_name: String,
    /// Pre-classified syntax spans over `text`.
    spans: Vec<SyntaxSpan>,
    /// Scroll handle for the raw text area.
    scroll: ScrollHandle,
    /// Parsed tree, when the value parses. `None` keeps the raw path only.
    tree: Option<StructuredNode>,
    /// Current body mode. Defaults to Tree when the tree parsed.
    mode: ViewerMode,
    /// Paths of collapsed branch nodes.
    collapsed: HashSet<String>,
    /// Live search query (tree mode).
    search: String,
    /// Search input in the toolbar.
    search_input: Entity<InputState>,
    /// Scroll handle for the tree area.
    tree_scroll: ScrollHandle,
    /// Flatten keyed by `(collapsed, search)`. Only the on-screen slice is painted.
    flatten_cache: FlattenCache,
    image: Option<Arc<gpui::Image>>,
    image_zoom: ImageZoom,
    image_size_label: String,
    geometry: Option<Arc<Geometry>>,
    embedded: bool,
    /// The original payload (byte-exact) when it is small enough to re-render
    /// per format choice; `None` hides the format picker.
    payload: Option<Vec<u8>>,
    /// The per-view display format. Auto restores `auto`.
    format: ValueFormat,
    /// The caveat a non-Auto render produced ("not valid JSON — …").
    format_note: Option<String>,
    /// The auto-detected view, captured before any format switch.
    auto: Option<AutoSnapshot>,
    /// Identity of the value in the session format memory (statement, row,
    /// column); explicit picks are remembered under it and restored when
    /// the same value is opened again.
    format_identity: Option<String>,
    /// What Auto's sniff decoded the payload as, if anything.
    auto_decoded: Option<ValueFormat>,
    /// The Protobuf message type picked for this value (custom schemas).
    message_choice: Option<crate::protobuf_schema::MessageChoice>,
    /// Where an edited, re-compressed value goes; `None` hides Edit.
    save_bytes: Option<SaveBytes>,
    /// Whether the shown decode can be edited and re-compressed: `Ok` with
    /// the framing, `Err` with the reason; `None` when the view is not a
    /// decompressed value at all.
    editable: Option<Result<crate::value_recompress::Framing, String>>,
    editor: Option<RecompressEditor>,
    /// "Staged …" after a re-compressed value was handed back.
    edit_status: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl StructuredDataViewer {
    pub fn new(
        column_name: &str,
        raw_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = detect(raw_text).unwrap_or(StructuredDataKind::Json);
        let text = pretty_print(raw_text, kind);
        let spans = classify(&text, kind);
        let tree = build_tree(raw_text, kind);
        let mode = initial_mode(kind, tree.is_some());
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.structured_data.tree_search_placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.search = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        Self {
            payload: (raw_text.len() <= FORMAT_PICKER_MAX_BYTES)
                .then(|| raw_text.as_bytes().to_vec()),
            format: ValueFormat::Auto,
            format_note: None,
            format_identity: None,
            auto_decoded: None,
            message_choice: None,
            save_bytes: None,
            editable: None,
            editor: None,
            edit_status: None,
            auto: Some(AutoSnapshot {
                text: text.clone(),
                kind,
                spans: spans.clone(),
                tree: tree.clone(),
                mode,
                note: None,
            }),
            text,
            kind,
            column_name: column_name.to_string(),
            spans,
            scroll: ScrollHandle::new(),
            tree,
            mode,
            collapsed: HashSet::new(),
            search: String::new(),
            search_input,
            tree_scroll: ScrollHandle::new(),
            flatten_cache: FlattenCache::new(),
            image: None,
            image_zoom: ImageZoom::Fit,
            image_size_label: String::new(),
            geometry: None,
            embedded: false,
            _subscriptions: subscriptions,
        }
    }

    /// Construct the viewer for a pre-rendered binary hex dump. Unlike
    /// [`Self::new`] there is no detection, tree, or syntax classification — the
    /// dump is shown verbatim in Raw mode.
    pub fn new_hex(
        column_name: &str,
        dump: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.structured_data.hex_search_placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.search = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        Self {
            payload: None,
            format: ValueFormat::Auto,
            format_note: None,
            format_identity: None,
            auto_decoded: None,
            message_choice: None,
            save_bytes: None,
            editable: None,
            editor: None,
            edit_status: None,
            auto: Some(AutoSnapshot {
                text: dump.clone(),
                kind: StructuredDataKind::Hex,
                spans: Vec::new(),
                tree: None,
                mode: ViewerMode::Raw,
                note: None,
            }),
            text: dump,
            kind: StructuredDataKind::Hex,
            column_name: column_name.to_string(),
            spans: Vec::new(),
            scroll: ScrollHandle::new(),
            tree: None,
            mode: ViewerMode::Raw,
            collapsed: HashSet::new(),
            search: String::new(),
            search_input,
            tree_scroll: ScrollHandle::new(),
            flatten_cache: FlattenCache::new(),
            image: None,
            image_zoom: ImageZoom::Fit,
            image_size_label: String::new(),
            geometry: None,
            embedded: false,
            _subscriptions: subscriptions,
        }
    }

    /// Hide the window title in the toolbar when the viewer is embedded in
    /// another chrome (the Cell Details dock).
    pub fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    /// Hand the viewer the byte-exact payload (enabling the format picker,
    /// e.g. for a `CellValue::Bytes` behind a hex view) and the value's
    /// identity in the session format memory. A format remembered for the
    /// identity is applied at once.
    pub fn set_payload(
        &mut self,
        payload: Vec<u8>,
        identity: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        if payload.len() <= FORMAT_PICKER_MAX_BYTES {
            self.payload = Some(payload);
        }
        self.format_identity = identity;
        self.message_choice = self
            .format_identity
            .as_deref()
            .and_then(crate::protobuf_schema::remembered_message);
        let remembered = self
            .format_identity
            .as_deref()
            .and_then(crate::structured_data::remembered_format);
        if let Some(format) = remembered
            && self.payload.is_some()
        {
            self.set_format(format, cx);
            return;
        }
        self.sniff_auto(cx);
    }

    /// Auto's second look: when the construction-time detection found
    /// nothing structured (plain text or a hex dump), sniff the payload
    /// for a compressed, encoded, or serialized format and show its decode
    /// as the Auto view, with a note saying what was found. Images and
    /// geometry keep their own views.
    fn sniff_auto(&mut self, cx: &mut gpui::Context<Self>) {
        if self.format != ValueFormat::Auto
            || self.tree.is_some()
            || self.image.is_some()
            || self.geometry.is_some()
            || matches!(
                self.kind,
                StructuredDataKind::Markdown | StructuredDataKind::Yaml
            )
        {
            self.refresh_editable();
            return;
        }
        let Some(payload) = self.payload.as_deref() else {
            return;
        };
        let Some((format, mut rendered)) = crate::structured_data::render_auto(payload) else {
            return;
        };
        if format == ValueFormat::Protobuf
            && let Some(choice) = &self.message_choice
        {
            rendered = crate::protobuf_schema::render_with_choice(payload, choice);
        }
        self.auto_decoded = Some(format);
        let mut snapshot = AutoSnapshot::from_rendered(&rendered);
        snapshot.note = Some(match snapshot.note.take() {
            Some(note) if note.starts_with(&format.localized_label()) => {
                crate::localization::format(
                    "results.structured_data.auto_existing_note",
                    &[("decode_note", &note)],
                )
            }
            Some(note) => crate::localization::format(
                "results.structured_data.auto_format_and_note",
                &[
                    ("format_name", &format.localized_label()),
                    ("decode_note", &note),
                ],
            ),
            None => crate::localization::format(
                "results.structured_data.auto_format_only",
                &[("format_name", &format.localized_label())],
            ),
        });
        self.apply_snapshot(&snapshot);
        self.auto = Some(snapshot);
        self.refresh_editable();
        cx.notify();
    }

    /// The format the shown view decoded: the explicit pick, or Auto's
    /// finding.
    fn decoded_format(&self) -> Option<ValueFormat> {
        if self.format == ValueFormat::Auto {
            self.auto_decoded
        } else {
            Some(self.format)
        }
    }

    /// Render the payload in `format`, through the picked message type when
    /// it is Protobuf and one is picked.
    fn render_current(&self, payload: &[u8], format: ValueFormat) -> RenderedPayload {
        match (&self.message_choice, format) {
            (Some(choice), ValueFormat::Protobuf) => {
                crate::protobuf_schema::render_with_choice(payload, choice)
            }
            _ => render_payload(payload, format),
        }
    }

    /// Recompute whether the shown decode can be edited and re-compressed.
    fn refresh_editable(&mut self) {
        self.editable = match (
            &self.payload,
            crate::value_recompress::compressed_format(self.decoded_format()),
        ) {
            (Some(payload), Some(format)) if self.save_bytes.is_some() => {
                Some(crate::value_recompress::editable(payload, format).map(|value| value.framing))
            }
            _ => None,
        };
    }

    /// Let the viewer hand an edited, re-compressed value back (the grid
    /// stages it as a cell edit).
    pub fn set_save_bytes(&mut self, save: SaveBytes, cx: &mut gpui::Context<Self>) {
        self.save_bytes = Some(save);
        self.refresh_editable();
        cx.notify();
    }

    /// Open the editor on the decompressed text.
    fn open_editor(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let (Some(payload), Some(format)) = (
            self.payload.clone(),
            crate::value_recompress::compressed_format(self.decoded_format()),
        ) else {
            return;
        };
        match crate::value_recompress::editable(&payload, format) {
            Ok(value) => {
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .soft_wrap(true)
                        .auto_grow(12, 40)
                });
                input.update(cx, |state, cx| state.set_value(value.text, window, cx));
                self.editor = Some(RecompressEditor {
                    framing: value.framing,
                    input,
                    error: None,
                });
                self.edit_status = None;
            }
            Err(error) => self.edit_status = Some(error),
        }
        cx.notify();
    }

    /// Re-compress the edited text in the original framing and hand it to
    /// the opener; the view then shows the new value.
    fn stage_edit(&mut self, cx: &mut gpui::Context<Self>) {
        let (Some(editor), Some(save)) = (self.editor.as_mut(), self.save_bytes.clone()) else {
            return;
        };
        let text = editor.input.read(cx).value().to_string();
        let framing = editor.framing;
        match crate::value_recompress::recompress(framing, &text) {
            Ok(bytes) => {
                self.editor = None;
                self.edit_status = Some(crate::localization::format(
                    "results.structured_data.recompress_staged",
                    &[("format_name", framing.label())],
                ));
                save(bytes.clone(), cx);
                self.payload = Some(bytes.clone());
                if let Some(format) = self.decoded_format() {
                    let rendered = self.render_current(&bytes, format);
                    let mut view = AutoSnapshot::from_rendered(&rendered);
                    if self.format == ValueFormat::Auto {
                        view.note = self.auto.as_ref().and_then(|auto| auto.note.clone());
                        self.auto = Some(view.clone());
                    }
                    self.apply_snapshot(&view);
                }
            }
            Err(error) => editor.error = Some(error),
        }
        cx.notify();
    }

    /// Pick (or with `None` clear) the Protobuf message type for this value.
    fn set_message_choice(
        &mut self,
        choice: Option<crate::protobuf_schema::MessageChoice>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.message_choice = choice.clone();
        let remember_error = self
            .format_identity
            .as_deref()
            .and_then(|identity| crate::protobuf_schema::remember_message(identity, choice).err());
        if self.format != ValueFormat::Protobuf {
            // Picking a message type is picking Protobuf.
            self.set_format(ValueFormat::Protobuf, cx);
        } else if let Some(payload) = self.payload.clone() {
            let rendered = self.render_current(&payload, ValueFormat::Protobuf);
            let view = AutoSnapshot::from_rendered(&rendered);
            self.apply_snapshot(&view);
        }
        if let Some(error) = remember_error {
            self.format_note = Some(error);
        }
        cx.notify();
    }

    fn apply_snapshot(&mut self, snapshot: &AutoSnapshot) {
        self.text = snapshot.text.clone();
        self.kind = snapshot.kind;
        self.spans = snapshot.spans.clone();
        self.tree = snapshot.tree.clone();
        self.mode = snapshot.mode;
        self.format_note = snapshot.note.clone();
        self.collapsed.clear();
        self.search.clear();
    }

    /// Switch the per-view display format. Auto restores the auto-detected
    /// view captured at construction (or sniffed from the payload);
    /// anything else re-renders the payload.
    pub fn set_format(&mut self, format: ValueFormat, cx: &mut gpui::Context<Self>) {
        if self.format == format {
            return;
        }
        self.format = format;
        // An unreadable remembered-formats file is never overwritten; say
        // why the choice will not outlast the session.
        let remember_error = self
            .format_identity
            .as_deref()
            .and_then(|identity| crate::structured_data::remember_format(identity, format).err());
        if format == ValueFormat::Auto {
            if let Some(auto) = self.auto.clone() {
                self.apply_snapshot(&auto);
            } else {
                self.format_note = None;
            }
        } else if let Some(payload) = self.payload.clone() {
            let rendered = self.render_current(&payload, format);
            let view = AutoSnapshot::from_rendered(&rendered);
            self.apply_snapshot(&view);
        }
        self.editor = None;
        self.edit_status = None;
        self.refresh_editable();
        if let Some(error) = remember_error {
            self.format_note = Some(match self.format_note.take() {
                Some(note) => crate::localization::format(
                    "results.structured_data.decode_and_persistence_notes",
                    &[("decode_note", &note), ("persistence_error", &error)],
                ),
                None => error,
            });
        }
        cx.notify();
    }

    pub fn new_image(
        column_name: &str,
        format: crate::structured_data::ImageFormat,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.structured_data.image_search_placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.search = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        let n = bytes.len();
        let dump = hex_dump(&bytes);
        let gpui_fmt = match format {
            crate::structured_data::ImageFormat::Png => gpui::ImageFormat::Png,
            crate::structured_data::ImageFormat::Jpeg => gpui::ImageFormat::Jpeg,
            crate::structured_data::ImageFormat::Gif => gpui::ImageFormat::Gif,
            crate::structured_data::ImageFormat::Webp => gpui::ImageFormat::Webp,
            crate::structured_data::ImageFormat::Bmp => gpui::ImageFormat::Bmp,
        };
        Self {
            payload: None,
            format: ValueFormat::Auto,
            format_note: None,
            format_identity: None,
            auto_decoded: None,
            message_choice: None,
            save_bytes: None,
            editable: None,
            editor: None,
            edit_status: None,
            auto: None,
            text: dump,
            kind: StructuredDataKind::Image,
            column_name: column_name.to_string(),
            spans: Vec::new(),
            scroll: ScrollHandle::new(),
            tree: None,
            mode: ViewerMode::Raw,
            collapsed: HashSet::new(),
            search: String::new(),
            search_input,
            tree_scroll: ScrollHandle::new(),
            flatten_cache: FlattenCache::new(),
            image: Some(Arc::new(gpui::Image::from_bytes(gpui_fmt, bytes))),
            image_zoom: ImageZoom::Fit,
            image_size_label: crate::localization::format(
                "results.structured_data.image_size_label",
                &[
                    ("image_format", format.display_name()),
                    ("byte_count", &n.to_string()),
                ],
            ),
            geometry: None,
            embedded: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn new_spatial(
        column_name: &str,
        raw_text: String,
        geometry: Geometry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.structured_data.spatial_search_placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.search = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        Self {
            payload: None,
            format: ValueFormat::Auto,
            format_note: None,
            format_identity: None,
            auto_decoded: None,
            message_choice: None,
            save_bytes: None,
            editable: None,
            editor: None,
            edit_status: None,
            auto: None,
            text: raw_text,
            kind: StructuredDataKind::Spatial,
            column_name: column_name.to_string(),
            spans: Vec::new(),
            scroll: ScrollHandle::new(),
            tree: None,
            mode: ViewerMode::Sketch,
            collapsed: HashSet::new(),
            search: String::new(),
            search_input,
            tree_scroll: ScrollHandle::new(),
            flatten_cache: FlattenCache::new(),
            image: None,
            image_zoom: ImageZoom::Fit,
            image_size_label: String::new(),
            geometry: Some(Arc::new(geometry)),
            embedded: false,
            _subscriptions: subscriptions,
        }
    }

    fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        cx.notify();
    }
}

/// Owned snapshot of one visible tree row, detached from the tree borrow so
/// render listeners can capture what they need.
#[derive(Clone)]
struct RowData {
    depth: usize,
    label: String,
    preview: Option<String>,
    path: String,
    is_branch: bool,
    child_count: usize,
    is_collapsed: bool,
    is_marker: bool,
}

/// Map a syntax token to a theme color.
fn token_color(token: SyntaxToken) -> gpui::Hsla {
    match token {
        SyntaxToken::Key => Theme::keyword(),
        SyntaxToken::String => Theme::string(),
        SyntaxToken::Number => Theme::number(),
        SyntaxToken::Boolean => Theme::keyword(),
        SyntaxToken::Null => Theme::comment(),
        SyntaxToken::Punctuation => Theme::text_dim(),
        SyntaxToken::ElementName => Theme::kind_table(),
        SyntaxToken::AttributeName => Theme::kind_column(),
        SyntaxToken::AttributeValue => Theme::string(),
        SyntaxToken::Comment => Theme::comment(),
        SyntaxToken::Text => Theme::text(),
    }
}

/// Color for a tree row label, by kind and label shape.
fn label_color(kind: StructuredDataKind, label: &str, is_marker: bool) -> gpui::Hsla {
    if is_marker || label == "#text" {
        return Theme::text_dim();
    }
    match kind {
        StructuredDataKind::Json | StructuredDataKind::Yaml => Theme::keyword(),
        StructuredDataKind::Xml => {
            if label.starts_with('@') {
                Theme::kind_column()
            } else {
                Theme::kind_table()
            }
        }
        // Hex dumps have no tree/label coloring; render plain.
        StructuredDataKind::Hex
        | StructuredDataKind::Image
        | StructuredDataKind::Spatial
        | StructuredDataKind::Markdown => Theme::text(),
    }
}

/// Render a single line of text split into colored segments by the
/// classified spans that overlap it.
fn render_line(
    line: &str,
    line_start: usize,
    _full_text: &str,
    spans: &[SyntaxSpan],
) -> impl IntoElement {
    let line_end = line_start + line.len();

    let mut segments: Vec<gpui::Div> = Vec::new();
    let mut cursor = line_start;

    for span in spans {
        if span.end <= line_start || span.start >= line_end {
            continue;
        }
        if span.start > cursor {
            let gap_start = cursor - line_start;
            let gap_end = span.start.min(line_end) - line_start;
            segments.push(
                div()
                    .text_color(Theme::text())
                    .child(line[gap_start..gap_end].to_string()),
            );
        }
        let seg_start = span.start.max(line_start) - line_start;
        let seg_end = span.end.min(line_end) - line_start;
        let color = token_color(span.token);
        segments.push(
            div()
                .text_color(color)
                .child(line[seg_start..seg_end].to_string()),
        );
        cursor = span.end.max(cursor);
    }

    if cursor < line_end {
        let remaining = &line[cursor - line_start..];
        segments.push(div().text_color(Theme::text()).child(remaining.to_string()));
    }

    div()
        .flex()
        .flex_row()
        .w_full()
        .min_w_full()
        .text_size(px(Theme::font_size()))
        .children(segments)
}

impl StructuredDataViewer {
    fn render_tree_row(
        &self,
        index: usize,
        row: &RowData,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        let group_name = SharedString::from(format!("sd-tree-row-{index}"));
        let indent = s(12.0) + row.depth as f32 * s(16.0);

        let chevron: gpui::Div = if row.is_branch {
            div()
                .w(px(s(14.0)))
                .flex_shrink_0()
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.85)))
                .child(if row.is_collapsed {
                    "\u{25b8}"
                } else {
                    "\u{25be}"
                })
        } else {
            div().w(px(s(14.0))).flex_shrink_0()
        };

        let mut inner = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .flex_1()
            .overflow_hidden()
            .child(chevron)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(label_color(self.kind, &row.label, row.is_marker))
                    .child(row.label.clone()),
            );
        if let Some(preview) = &row.preview {
            inner = inner.child(
                div()
                    .truncate()
                    .text_color(Theme::text_dim())
                    .child(preview.clone()),
            );
        } else if row.is_branch {
            inner = inner.child(
                div()
                    .flex_shrink_0()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(format!("({})", row.child_count)),
            );
        }

        // Hover affordances: Copy Path always (except markers); Copy Value
        // only when the row carries a scalar value.
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(4.0)))
            .flex_shrink_0()
            .invisible()
            .group_hover(group_name.clone(), |s| s.visible());
        if !row.is_marker {
            let path = row.path.clone();
            actions = actions.child(
                div()
                    .id(SharedString::from(format!("sd-copy-path-{index}")))
                    .cursor_pointer()
                    .px(px(s(6.0)))
                    .py(px(s(1.0)))
                    .rounded(px(s(3.0)))
                    .bg(Theme::accent().opacity(0.12))
                    .text_color(Theme::accent())
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .child(crate::localization::text(
                        "results.structured_data.tree_copy_path",
                    ))
                    .on_click(cx.listener(move |_, _event, _window, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    })),
            );
            if let Some(value) = row.preview.clone() {
                actions = actions.child(
                    div()
                        .id(SharedString::from(format!("sd-copy-value-{index}")))
                        .cursor_pointer()
                        .px(px(s(6.0)))
                        .py(px(s(1.0)))
                        .rounded(px(s(3.0)))
                        .bg(Theme::accent().opacity(0.12))
                        .text_color(Theme::accent())
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .child(crate::localization::text(
                            "results.structured_data.tree_copy_value",
                        ))
                        .on_click(cx.listener(move |_, _event, _window, cx| {
                            cx.stop_propagation();
                            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                        })),
                );
            }
        }

        let mut row_div = div()
            .id(SharedString::from(format!("sd-tree-row-id-{index}")))
            .group(group_name)
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .flex_shrink_0()
            .pl(px(indent))
            .pr(px(s(12.0)))
            .py(px(s(1.0)))
            .text_size(px(Theme::font_size()))
            .hover(|st| st.bg(Theme::row_hover()))
            .child(inner)
            .child(actions);

        if row.is_branch {
            let path = row.path.clone();
            row_div = row_div.cursor_pointer().on_click(cx.listener(
                move |this: &mut Self, _event, _window, cx| {
                    this.toggle_collapsed(&path, cx);
                },
            ));
        }
        row_div
    }

    fn render_tree_body(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        if let Some(tree) = self.tree.as_ref() {
            self.flatten_cache
                .refresh(tree, &self.collapsed, &self.search);
        } else {
            self.flatten_cache.clear();
        }

        let searching = !self.search.trim().is_empty();
        let truncated = self.flatten_cache.truncated;
        let row_count = self.flatten_cache.rows.len();

        let mut body = div()
            .id("structured-data-tree-body")
            .track_scroll(&self.tree_scroll)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .bg(Theme::bg())
            .py(px(s(8.0)));

        if row_count == 0 && searching {
            body = body.child(
                div()
                    .px(px(s(16.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.structured_data.tree_no_matches",
                    )),
            );
            return body;
        }

        let viewport_h = f32::from(self.tree_scroll.bounds().size.height);
        let offset_y = f32::from(self.tree_scroll.offset().y);
        let row_h = Theme::row_height();
        let (win_start, win_end) = tree_window_bounds(viewport_h, offset_y, row_h, row_count);
        let window_rows: Vec<(usize, RowData)> = self.flatten_cache.rows[win_start..win_end]
            .iter()
            .cloned()
            .enumerate()
            .map(|(j, row)| (win_start + j, row))
            .collect();

        if win_start > 0 {
            body = body.child(div().h(px(row_h * win_start as f32)));
        }
        body = body.children(
            window_rows
                .iter()
                .map(|(i, row)| self.render_tree_row(*i, row, cx)),
        );
        if win_end < row_count {
            body = body.child(div().h(px(row_h * (row_count - win_end) as f32)));
        }

        if truncated > 0 {
            body = body.child(
                div()
                    .px(px(s(16.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.structured_data.tree_display_truncated",
                        &[("hidden_rows", &truncated.to_string())],
                    )),
            );
        }
        body
    }

    /// One segment of the Raw/Tree toggle in the toolbar.
    fn mode_button(
        &self,
        id: &'static str,
        label: String,
        mode: ViewerMode,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        let active = self.mode == mode;
        div()
            .id(SharedString::from(format!("sd-mode-{id}")))
            .cursor_pointer()
            .px(px(s(10.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(label)
            .on_click(cx.listener(move |this: &mut Self, _event, _window, cx| {
                if this.mode != mode {
                    this.mode = mode;
                    cx.notify();
                }
            }))
    }

    /// The format picker: a dropdown naming the current format, with every
    /// format grouped (plain, encoded, compressed, serialized). The
    /// current one is checked; a format this build cannot decode (zstd in
    /// the browser) is disabled rather than hidden.
    fn format_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let current = self.format;
        Button::new("sd-format-menu")
            .ghost()
            .xsmall()
            .label(crate::localization::format(
                "results.structured_data.format_picker_label",
                &[("format_name", &current.localized_label())],
            ))
            .app_tooltip_text(crate::localization::text(
                "results.structured_data.format_picker_tooltip",
            ))
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu;
                for (index, group) in FormatGroup::ALL.into_iter().enumerate() {
                    if index > 0 {
                        menu = menu.separator();
                    }
                    for format in ValueFormat::ALL
                        .into_iter()
                        .filter(|format| format.group() == group)
                    {
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format.localized_label())
                                .checked(format == current)
                                .disabled(!format.available())
                                .on_click(move |_event, _window, app| {
                                    this.update(app, |viewer, cx| viewer.set_format(format, cx));
                                }),
                        );
                    }
                }
                menu
            })
    }

    /// The Protobuf message-type picker: none (field numbers) or any message
    /// from a registered schema. Shown only while the view is a Protobuf
    /// decode and at least one schema is registered.
    fn message_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let schemas = crate::protobuf_schema::schemas();
        let current = self.message_choice.clone();
        let label = match &current {
            Some(choice) => crate::localization::format(
                "results.structured_data.message_picker_label",
                &[("message", &choice.message)],
            ),
            None => crate::localization::format(
                "results.structured_data.message_picker_label",
                &[(
                    "message",
                    &crate::localization::text("results.structured_data.message_picker_none"),
                )],
            ),
        };
        Button::new("sd-message-menu")
            .ghost()
            .xsmall()
            .label(label)
            .app_tooltip_text(crate::localization::text(
                "results.structured_data.message_picker_tooltip",
            ))
            .dropdown_menu(move |menu, _window, _cx| {
                let none_this = this.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(crate::localization::text(
                        "results.structured_data.message_picker_none",
                    ))
                    .checked(current.is_none())
                    .on_click(move |_event, _window, app| {
                        none_this.update(app, |viewer, cx| viewer.set_message_choice(None, cx));
                    }),
                );
                for summary in &schemas {
                    let Ok(messages) = &summary.messages else {
                        continue;
                    };
                    menu = menu.separator();
                    for message in messages {
                        let choice = crate::protobuf_schema::MessageChoice {
                            schema_id: summary.entry.id.clone(),
                            message: message.clone(),
                        };
                        let checked = current.as_ref() == Some(&choice);
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::protobuf_schema::choice_label(
                                &choice, &schemas,
                            ))
                            .checked(checked)
                            .on_click(move |_event, _window, app| {
                                let choice = choice.clone();
                                this.update(app, |viewer, cx| {
                                    viewer.set_message_choice(Some(choice), cx)
                                });
                            }),
                        );
                    }
                }
                menu
            })
    }

    fn zoom_button(
        &self,
        id: &'static str,
        label: String,
        zoom: ImageZoom,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        let active = self.image_zoom == zoom;
        div()
            .id(SharedString::from(format!("sd-zoom-{id}")))
            .cursor_pointer()
            .px(px(s(10.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(label)
            .on_click(cx.listener(move |this: &mut Self, _event, _window, cx| {
                if this.image_zoom != zoom {
                    this.image_zoom = zoom;
                    cx.notify();
                }
            }))
    }

    fn render_image_body(&self) -> gpui::AnyElement {
        let Some(image) = self.image.clone() else {
            return div().flex_1().into_any_element();
        };
        match self.image_zoom {
            ImageZoom::Fit => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(img(image).object_fit(ObjectFit::Contain).size_full())
                .into_any_element(),
            ImageZoom::Actual => div()
                .id("image-scroll")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .child(img(image).object_fit(ObjectFit::None))
                .into_any_element(),
        }
    }

    fn render_spatial_body(&self) -> gpui::Div {
        let Some(geometry) = self.geometry.clone() else {
            return div().flex_1();
        };
        let summary = geometry.summary();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .px(px(Theme::scaled(12.0)))
                    .py(px(Theme::scaled(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(summary),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(Theme::border())
                    .child(spatial_canvas(geometry)),
            )
    }
}

impl Render for StructuredDataViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let title = if self.format == ValueFormat::Auto {
            crate::localization::format(
                "results.structured_data.auto_window_title",
                &[
                    ("kind_name", &self.kind.localized_display_name()),
                    ("column_name", &self.column_name),
                ],
            )
        } else {
            crate::localization::format(
                "results.structured_data.format_window_title",
                &[
                    ("format_name", &self.format.localized_label()),
                    ("column_name", &self.column_name),
                ],
            )
        };
        let has_tree = self.tree.is_some();
        let in_tree_mode = has_tree && self.mode == ViewerMode::Tree;

        let mut left = div().flex().flex_row().items_center().gap(px(s(10.0)));
        if !self.embedded {
            left = left.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(title),
            );
        }
        // The format picker: per-view (a Redis key's choice also remembered
        // on disk), only when the raw payload is at hand. Images and
        // geometry keep their own views.
        let picker_kinds = !matches!(
            self.kind,
            StructuredDataKind::Image | StructuredDataKind::Spatial
        );
        if self.payload.is_some() && picker_kinds {
            left = left.child(self.format_picker(cx));
            if self.decoded_format() == Some(ValueFormat::Protobuf)
                && crate::protobuf_schema::any_schemas()
            {
                left = left.child(self.message_picker(cx));
            }
            if let Some(editable) = &self.editable
                && self.editor.is_none()
            {
                let button = Button::new("sd-recompress-edit").ghost().xsmall().label(
                    crate::localization::text("results.structured_data.edit_button"),
                );
                left = left.child(match editable {
                    Ok(framing) => button
                        .app_tooltip_text(crate::localization::format(
                            "results.structured_data.edit_tooltip",
                            &[("format_name", framing.label())],
                        ))
                        .on_click(cx.listener(|this: &mut Self, _event, window, cx| {
                            this.open_editor(window, cx);
                        })),
                    Err(reason) => button.disabled(true).app_tooltip_text(reason.clone()),
                });
            }
            if let Some(status) = &self.edit_status {
                left = left.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(status.clone()),
                );
            }
            if let Some(note) = &self.format_note {
                left = left.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::warning())
                        .child(note.clone()),
                );
            }
        }
        if has_tree {
            left = left.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .p(px(s(2.0)))
                    .rounded(px(s(5.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .child(self.mode_button(
                        "tree",
                        crate::localization::text("results.structured_data.tree_mode_button"),
                        ViewerMode::Tree,
                        cx,
                    ))
                    .child(self.mode_button(
                        "raw",
                        crate::localization::text("results.structured_data.raw_mode_button"),
                        ViewerMode::Raw,
                        cx,
                    )),
            );
        } else if self.kind == StructuredDataKind::Markdown {
            left = left.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .p(px(s(2.0)))
                    .rounded(px(s(5.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .child(self.mode_button(
                        "preview",
                        crate::localization::text("results.structured_data.preview_mode_button"),
                        ViewerMode::Preview,
                        cx,
                    ))
                    .child(self.mode_button(
                        "raw",
                        crate::localization::text("results.structured_data.raw_mode_button"),
                        ViewerMode::Raw,
                        cx,
                    )),
            );
        } else if self.kind == StructuredDataKind::Image {
            left = left.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .p(px(s(2.0)))
                    .rounded(px(s(5.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .child(self.zoom_button(
                        "fit",
                        crate::localization::text("results.structured_data.image_fit_button"),
                        ImageZoom::Fit,
                        cx,
                    ))
                    .child(self.zoom_button("actual", "100%".to_string(), ImageZoom::Actual, cx)),
            );
        } else if self.kind == StructuredDataKind::Spatial {
            left = left.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .p(px(s(2.0)))
                    .rounded(px(s(5.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .child(self.mode_button(
                        "sketch",
                        crate::localization::text("results.structured_data.sketch_mode_button"),
                        ViewerMode::Sketch,
                        cx,
                    ))
                    .child(self.mode_button(
                        "map",
                        crate::localization::text("map_view.viewer.map_mode_button"),
                        ViewerMode::Map,
                        cx,
                    ))
                    .child(self.mode_button(
                        "raw",
                        crate::localization::text(
                            "results.structured_data.spatial_raw_mode_button",
                        ),
                        ViewerMode::Raw,
                        cx,
                    )),
            );
        }

        let mut right = div().flex().flex_row().items_center().gap(px(s(8.0)));
        if in_tree_mode {
            right = right.child(div().w(px(s(180.0))).child(Input::new(&self.search_input)));
        }
        if self.kind == StructuredDataKind::Image && !self.image_size_label.is_empty() {
            right = right.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(self.image_size_label.clone()),
            );
        }
        if let Some(editor) = &self.editor {
            if let Some(error) = &editor.error {
                right = right.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::warning())
                        .child(error.clone()),
                );
            }
            right = right
                .child(
                    Button::new("sd-recompress-cancel")
                        .ghost()
                        .xsmall()
                        .label(crate::localization::text(
                            "results.structured_data.cancel_edit_button",
                        ))
                        .on_click(cx.listener(|this: &mut Self, _event, _window, cx| {
                            this.editor = None;
                            cx.notify();
                        })),
                )
                .child(
                    crate::theme::accent_pill_button(
                        "sd-recompress-stage",
                        crate::localization::format(
                            "results.structured_data.stage_button",
                            &[("format_name", editor.framing.label())],
                        ),
                    )
                    .on_click(cx.listener(
                        |this: &mut Self, _event, _window, cx| {
                            this.stage_edit(cx);
                        },
                    )),
                );
        }
        let copy_text = self.text.clone();
        right = right.child(
            crate::theme::accent_pill_button(
                "copy-button",
                crate::localization::text("results.structured_data.copy_button"),
            )
            .on_click(cx.listener(move |_, _event, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
            })),
        );

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(s(16.0)))
            .py(px(s(8.0)))
            .bg(Theme::panel_bg())
            .border_b(px(1.0))
            .border_color(Theme::border())
            .child(left)
            .child(right);

        let body = if let Some(editor) = &self.editor {
            div()
                .id("structured-data-editor")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .bg(Theme::bg())
                .px(px(s(16.0)))
                .py(px(s(12.0)))
                .child(Textarea::new(&editor.input).w_full())
                .into_any_element()
        } else if self.kind == StructuredDataKind::Markdown && self.mode == ViewerMode::Preview {
            div()
                .id("structured-data-markdown")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .bg(Theme::bg())
                .px(px(s(16.0)))
                .py(px(s(12.0)))
                .child(
                    gpui_component::text::TextView::markdown(
                        "structured-data-markdown-view",
                        SharedString::from(self.text.clone()),
                    )
                    .selectable(true),
                )
                .into_any_element()
        } else if self.kind == StructuredDataKind::Image {
            self.render_image_body().into_any_element()
        } else if self.kind == StructuredDataKind::Spatial && self.mode == ViewerMode::Map {
            crate::map_view::viewer_map_body(self.geometry.clone(), &self.column_name, window, cx)
        } else if self.kind == StructuredDataKind::Spatial && self.mode != ViewerMode::Raw {
            self.render_spatial_body().into_any_element()
        } else if in_tree_mode {
            self.render_tree_body(cx).into_any_element()
        } else {
            let lines: Vec<&str> = self.text.lines().collect();
            let mut line_start = 0usize;
            div()
                .id("structured-data-body")
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .w_full()
                .h_full()
                .overflow_y_scroll()
                .bg(Theme::bg())
                .px(px(s(16.0)))
                .py(px(s(12.0)))
                .gap(px(0.0))
                .children(lines.iter().map(|line| {
                    let line_len = line.len();
                    let element = render_line(line, line_start, &self.text, &self.spans);
                    line_start += line_len + 1;
                    element.into_any_element()
                }))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .child(toolbar)
            .child(body)
    }
}

/// Map a classified preview to the viewer kind (pure, no window needed).
pub(crate) fn kind_for_preview(preview: &ContentPreview) -> StructuredDataKind {
    match preview {
        ContentPreview::Json => StructuredDataKind::Json,
        ContentPreview::Xml => StructuredDataKind::Xml,
        ContentPreview::Yaml => StructuredDataKind::Yaml,
        ContentPreview::Markdown => StructuredDataKind::Markdown,
        ContentPreview::Image(_, _) => StructuredDataKind::Image,
        ContentPreview::Spatial(_) => StructuredDataKind::Spatial,
        ContentPreview::Hex => StructuredDataKind::Hex,
        ContentPreview::Text => StructuredDataKind::Json,
    }
}

/// Open a viewer window for a classified cell. Image/spatial/hex/json/xml
/// each pick the matching constructor; plain text falls through to `new`.
pub fn open_cell_preview(column_name: &str, preview: ContentPreview, raw_text: &str, cx: &mut App) {
    // The format picker needs the byte-exact payload; for text cells the
    // string IS the payload (the driver only builds it from valid UTF-8),
    // and images carry their own bytes. A hex preview's bytes are unknown
    // to this path — its picker stays hidden unless the caller passes the
    // payload through `open_cell_preview_with_payload`.
    let payload = match &preview {
        ContentPreview::Image(_, bytes) => Some(bytes.clone()),
        ContentPreview::Json
        | ContentPreview::Xml
        | ContentPreview::Yaml
        | ContentPreview::Markdown
        | ContentPreview::Text => Some(raw_text.as_bytes().to_vec()),
        ContentPreview::Spatial(_) | ContentPreview::Hex => None,
    };
    open_cell_preview_with_payload(column_name, preview, raw_text, payload, None, cx);
}

/// [`open_cell_preview`] with the byte-exact payload the format picker
/// re-renders from. `None` (or an oversized payload) hides the picker and
/// the viewer behaves exactly as before. `identity` keys the session
/// format memory (see [`crate::structured_data::cell_format_identity`]).
pub fn open_cell_preview_with_payload(
    column_name: &str,
    preview: ContentPreview,
    raw_text: &str,
    payload: Option<Vec<u8>>,
    identity: Option<String>,
    cx: &mut App,
) {
    open_cell_preview_with_save(column_name, preview, raw_text, payload, identity, None, cx);
}

/// [`open_cell_preview_with_payload`] whose viewer can hand an edited,
/// re-compressed value back through `save` (the grid stages it).
pub fn open_cell_preview_with_save(
    column_name: &str,
    preview: ContentPreview,
    raw_text: &str,
    payload: Option<Vec<u8>>,
    identity: Option<String>,
    save: Option<SaveBytes>,
    cx: &mut App,
) {
    let kind = kind_for_preview(&preview);
    let title = crate::localization::format(
        "results.structured_data.detached_window_title",
        &[
            ("kind_name", &kind.localized_display_name()),
            ("column_name", &column_name.to_string()),
        ],
    );
    let bounds = Bounds::centered(None, size(px(720.), px(560.)), cx);
    let column_name = column_name.to_string();
    let raw_text = raw_text.to_string();
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(title)),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        move |window, cx| {
            let column_name = column_name.clone();
            let raw_text = raw_text.clone();
            let viewer = match preview {
                ContentPreview::Json
                | ContentPreview::Xml
                | ContentPreview::Yaml
                | ContentPreview::Markdown
                | ContentPreview::Text => {
                    cx.new(|cx| StructuredDataViewer::new(&column_name, &raw_text, window, cx))
                }
                ContentPreview::Image(fmt, bytes) => cx.new(|cx| {
                    StructuredDataViewer::new_image(&column_name, fmt, bytes, window, cx)
                }),
                ContentPreview::Spatial(geom) => cx.new(|cx| {
                    StructuredDataViewer::new_spatial(&column_name, raw_text, geom, window, cx)
                }),
                ContentPreview::Hex => {
                    cx.new(|cx| StructuredDataViewer::new_hex(&column_name, raw_text, window, cx))
                }
            };
            if let Some(save) = save.clone() {
                viewer.update(cx, |viewer, cx| viewer.set_save_bytes(save, cx));
            }
            if let Some(payload) = payload {
                let identity = identity.clone();
                viewer.update(cx, |viewer, cx| viewer.set_payload(payload, identity, cx));
            }
            cx.new(|cx| gpui_component::Root::new(viewer, window, cx))
        },
    );
}

/// Open a new GPUI window displaying the structured data viewer for `raw_text`.
/// Callable from any context that has `&mut App` (e.g. a context-menu action
/// handler). Does nothing if the cell is not JSON, XML, an image data URL, or
/// recognised geometry.
#[allow(dead_code)] // Kept as a thin wrapper for plan 183 / hydration callers.
pub fn open_structured_data_viewer(column_name: &str, raw_text: &str, cx: &mut App) {
    let preview = classify_cell(&CellValue::String(raw_text.to_string()));
    match preview {
        ContentPreview::Text | ContentPreview::Hex => {}
        other => open_cell_preview(column_name, other, raw_text, cx),
    }
}

/// Open a standalone viewer for a binary/varbinary cell value. Image and WKB
/// payloads get their specialised views; everything else is a hex dump.
#[allow(dead_code)] // Kept as a thin wrapper for plan 183 / hydration callers.
pub fn open_binary_viewer(column_name: &str, bytes: &[u8], cx: &mut App) {
    let preview = classify_cell(&CellValue::Bytes(bytes.to_vec()));
    let raw_text = hex_dump(bytes);
    open_cell_preview_with_payload(
        column_name,
        preview,
        &raw_text,
        Some(bytes.to_vec()),
        None,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    #[gpui::test]
    fn markdown_opens_in_preview_and_yaml_in_tree(cx: &mut gpui::TestAppContext) {
        init(cx);
        let (markdown, cx) = cx.add_window_view(|window, cx| {
            StructuredDataViewer::new("notes", "# Notes\n\n- one\n- two", window, cx)
        });
        cx.run_until_parked();
        markdown.read_with(cx, |viewer, _| {
            assert_eq!(viewer.kind, StructuredDataKind::Markdown);
            assert_eq!(viewer.mode, ViewerMode::Preview);
            assert!(!viewer.spans.is_empty());
        });
        let yaml = cx.update(|window, cx| {
            cx.new(|cx| {
                StructuredDataViewer::new("config", "name: api\nports:\n  - 80", window, cx)
            })
        });
        yaml.read_with(cx, |viewer, _| {
            assert_eq!(viewer.kind, StructuredDataKind::Yaml);
            assert_eq!(viewer.mode, ViewerMode::Tree);
            assert!(viewer.tree.is_some());
        });
    }

    #[gpui::test]
    fn an_edited_compressed_value_is_handed_back_recompressed(cx: &mut gpui::TestAppContext) {
        init(cx);
        let stored = crate::value_recompress::Framing::Zlib
            .encode(b"{\"n\":1}")
            .unwrap();
        let received: std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>> = Default::default();
        let sink = received.clone();
        let save: SaveBytes = std::rc::Rc::new(move |bytes, _cx| sink.borrow_mut().push(bytes));
        let dump = hex_dump(&stored);
        let (viewer, cx) = cx
            .add_window_view(|window, cx| StructuredDataViewer::new_hex("blob", dump, window, cx));
        viewer.update(cx, |viewer, cx| {
            viewer.set_save_bytes(save, cx);
            viewer.set_payload(stored.clone(), None, cx);
        });
        cx.run_until_parked();
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(viewer.auto_decoded, Some(ValueFormat::Deflate));
            assert_eq!(
                viewer.editable,
                Some(Ok(crate::value_recompress::Framing::Zlib))
            );
        });
        viewer.update_in(cx, |viewer, window, cx| {
            viewer.open_editor(window, cx);
            let input = viewer.editor.as_ref().expect("editor open").input.clone();
            assert_eq!(input.read(cx).value().to_string(), "{\"n\":1}");
            input.update(cx, |state, cx| state.set_value("{\"n\":2}", window, cx));
            viewer.stage_edit(cx);
            assert!(viewer.editor.is_none());
            assert!(viewer.edit_status.is_some());
        });
        let sent = received.borrow();
        assert_eq!(sent.len(), 1);
        assert!(crate::structured_data::codecs::looks_like_zlib(&sent[0]));
        assert_eq!(
            crate::value_recompress::Framing::Zlib
                .decode(&sent[0])
                .unwrap(),
            b"{\"n\":2}"
        );
    }

    #[gpui::test]
    fn without_a_save_target_or_a_known_framing_there_is_no_edit(cx: &mut gpui::TestAppContext) {
        init(cx);
        let stored = crate::value_recompress::Framing::Gzip
            .encode(b"text")
            .unwrap();
        let (viewer, cx) = cx.add_window_view(|window, cx| {
            StructuredDataViewer::new_hex("blob", hex_dump(&stored), window, cx)
        });
        viewer.update(cx, |viewer, cx| {
            viewer.set_payload(stored.clone(), None, cx)
        });
        viewer.read_with(cx, |viewer, _| assert_eq!(viewer.editable, None));
        // A save target, but the bytes are not compressed at all.
        viewer.update(cx, |viewer, cx| {
            viewer.set_save_bytes(std::rc::Rc::new(|_, _| {}), cx);
            viewer.set_format(ValueFormat::Gzip, cx);
            viewer.payload = Some(b"not gzip".to_vec());
            viewer.refresh_editable();
            assert!(matches!(viewer.editable, Some(Err(_))));
        });
    }

    #[test]
    fn viewer_classifies_json() {
        let raw = r#"{"a":1,"b":true}"#;
        let kind = detect(raw).unwrap();
        assert_eq!(kind, StructuredDataKind::Json);
        let pretty = pretty_print(raw, kind);
        let spans = classify(&pretty, kind);
        assert!(spans.iter().any(|s| s.token == SyntaxToken::Key));
        assert!(spans.iter().any(|s| s.token == SyntaxToken::Number));
        assert!(spans.iter().any(|s| s.token == SyntaxToken::Boolean));
    }

    #[test]
    fn viewer_classifies_xml() {
        let raw = "<root><child attr=\"val\">text</child></root>";
        let kind = detect(raw).unwrap();
        assert_eq!(kind, StructuredDataKind::Xml);
        let pretty = pretty_print(raw, kind);
        let spans = classify(&pretty, kind);
        assert!(spans.iter().any(|s| s.token == SyntaxToken::ElementName));
        assert!(spans.iter().any(|s| s.token == SyntaxToken::AttributeName));
        assert!(spans.iter().any(|s| s.token == SyntaxToken::AttributeValue));
    }

    #[test]
    fn open_viewer_ignores_non_structured() {
        // Can't easily test App context in unit tests, but we can verify
        // that detect returns None for plain text.
        assert!(detect("just some text").is_none());
        assert!(detect("SELECT * FROM foo").is_none());
        assert!(detect("").is_none());
    }

    #[test]
    fn open_cell_preview_maps_every_variant_to_a_kind() {
        assert_eq!(
            kind_for_preview(&ContentPreview::Json),
            StructuredDataKind::Json
        );
        assert_eq!(
            kind_for_preview(&ContentPreview::Xml),
            StructuredDataKind::Xml
        );
        assert_eq!(
            kind_for_preview(&ContentPreview::Image(
                crate::structured_data::ImageFormat::Png,
                vec![]
            )),
            StructuredDataKind::Image
        );
        let geom = sqlly_core::spatial::parse_wkt("POINT(1 2)").unwrap();
        assert_eq!(
            kind_for_preview(&ContentPreview::Spatial(geom)),
            StructuredDataKind::Spatial
        );
        assert_eq!(
            kind_for_preview(&ContentPreview::Hex),
            StructuredDataKind::Hex
        );
        assert_eq!(
            kind_for_preview(&ContentPreview::Text),
            StructuredDataKind::Json
        );
    }

    #[test]
    fn token_color_mapping_covers_all_variants() {
        for token in [
            SyntaxToken::Key,
            SyntaxToken::String,
            SyntaxToken::Number,
            SyntaxToken::Boolean,
            SyntaxToken::Null,
            SyntaxToken::Punctuation,
            SyntaxToken::ElementName,
            SyntaxToken::AttributeName,
            SyntaxToken::AttributeValue,
            SyntaxToken::Comment,
            SyntaxToken::Text,
        ] {
            let _ = token_color(token);
        }
    }

    #[test]
    fn render_line_splits_segments_at_span_boundaries() {
        let text = r#"{"key": 42}"#;
        let kind = detect(text).unwrap();
        let spans = classify(text, kind);
        let line = text;
        let line_start = 0;
        let line_end = line_start + line.len();

        let overlapping: Vec<&SyntaxSpan> = spans
            .iter()
            .filter(|s| s.end > line_start && s.start < line_end)
            .collect();
        assert!(!overlapping.is_empty());
    }

    #[test]
    fn pretty_print_preserves_structure() {
        let raw = r#"{"z":1,"a":2}"#;
        let kind = detect(raw).unwrap();
        let pretty = pretty_print(raw, kind);
        assert!(pretty.contains("\"a\""));
        assert!(pretty.contains("\"z\""));
        assert!(pretty.contains("\n"));
    }

    #[test]
    fn tree_parses_for_valid_json_and_not_for_plain_text() {
        assert!(build_tree(r#"{"a":1}"#, StructuredDataKind::Json).is_some());
        assert!(build_tree("plain text", StructuredDataKind::Json).is_none());
        assert!(build_tree("<a><b></a>", StructuredDataKind::Xml).is_none());
    }

    fn json_object_with_keys(n: usize) -> String {
        let mut s = String::from("{");
        for i in 0..n {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("\"k{i}\":{i}"));
        }
        s.push('}');
        s
    }

    #[test]
    fn tree_window_bounds_small_viewport_on_200_rows() {
        let row_h = 20.0;
        let viewport_h = 100.0;
        let row_count = 200;
        let (start, end) = tree_window_bounds(viewport_h, 0.0, row_h, row_count);
        // ceil(100/20) + 6 overscan = 11, matching schema-tree math.
        assert_eq!((start, end), (0, 11));
        assert!(end < row_count);

        let (start, end) = tree_window_bounds(viewport_h, -400.0, row_h, row_count);
        // scroll_y=400 → first=20 → start=18; end=18+11=29.
        assert_eq!((start, end), (18, 29));
    }

    #[test]
    fn tree_window_bounds_zero_viewport_does_not_materialize_all_rows() {
        let (start, end) = tree_window_bounds(0.0, 0.0, 20.0, 200);
        assert_eq!(start, 0);
        assert!(end > 0, "first paint still shows an overscan slice");
        assert!(
            end < 200,
            "viewport height 0 must not build every flattened row"
        );
        assert_eq!(end, 6);
    }

    #[test]
    fn flatten_cache_hits_on_same_collapsed_and_search() {
        let json = json_object_with_keys(20);
        let root = build_tree(&json, StructuredDataKind::Json).unwrap();
        let collapsed = HashSet::new();
        let mut cache = FlattenCache::new();
        assert!(!cache.refresh(&root, &collapsed, ""));
        let ptr = cache.rows.as_ptr();
        assert!(cache.refresh(&root, &collapsed, ""), "same key is a hit");
        assert_eq!(cache.rows.as_ptr(), ptr);

        assert!(!cache.refresh(&root, &collapsed, "k1"));
        assert_ne!(cache.rows.as_ptr(), ptr);
        assert!(cache.rows.iter().any(|r| r.label == "k1"));
    }

    #[test]
    fn collapse_hides_descendants_in_cached_flatten() {
        let json = r#"{"a":{"x":1,"y":2},"b":3}"#;
        let root = build_tree(json, StructuredDataKind::Json).unwrap();
        let (expanded, _) = collect_visible_rows(&root, &HashSet::new(), "");
        let labels: Vec<&str> = expanded.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["$", "a", "x", "y", "b"]);

        let mut collapsed = HashSet::new();
        collapsed.insert("$.a".to_string());
        let (visible, _) = collect_visible_rows(&root, &collapsed, "");
        let labels: Vec<&str> = visible.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["$", "a", "b"]);
    }

    #[test]
    fn search_finds_nodes_outside_the_render_window() {
        let json = json_object_with_keys(199);
        let root = build_tree(&json, StructuredDataKind::Json).unwrap();
        let (all, truncated) = collect_visible_rows(&root, &HashSet::new(), "");
        assert_eq!(truncated, 0);
        assert_eq!(all.len(), 200);

        let k150_index = all.iter().position(|r| r.label == "k150").unwrap();
        let (_win_start, win_end) = tree_window_bounds(80.0, 0.0, 20.0, all.len());
        assert!(
            k150_index >= win_end,
            "k150 sits past the first-window end ({win_end}), at {k150_index}"
        );

        let (found, _) = collect_visible_rows(&root, &HashSet::new(), "k150");
        assert!(
            found.iter().any(|r| r.label == "k150"),
            "search still finds the matching node"
        );
        assert_eq!(found[0].label, "$", "ancestors stay in the search flatten");
    }
}
