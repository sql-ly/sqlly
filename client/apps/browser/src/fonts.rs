//! Interface fonts for the browser build, which has no system fonts.
//!
//! Two subsets ship inside the wasm binary for every visitor: the default UI
//! face (Noto Sans SC: Latin plus common Simplified Chinese) and Noto Sans
//! (Latin, Greek, Cyrillic), which is also gpui's fallback family. Japanese,
//! Korean and Traditional Chinese each need a face of their own, both for
//! coverage and for the regional glyph shapes of shared ideographs, so those
//! are fetched from `fonts/` beside the page the first time the interface
//! switches to that language. Until the face arrives the embedded subsets and
//! the browser's own fonts (gpui's Canvas fallback) draw the text.
//!
//! `fonts/build-fonts.py` builds every file; `tests/font_coverage.rs` checks
//! each catalog against the face its language uses.

/// One interface face the browser shell registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceFont {
    /// The family name inside the font file.
    pub family: &'static str,
    /// The file in `apps/browser/fonts` (and `dist/fonts`).
    pub file: &'static str,
    /// Compiled into the wasm binary rather than fetched on first use.
    pub embedded: bool,
}

pub const SIMPLIFIED_CHINESE: InterfaceFont = InterfaceFont {
    family: "Noto Sans SC",
    file: "NotoSansSC-Regular-subset.ttf",
    embedded: true,
};
pub const LATIN: InterfaceFont = InterfaceFont {
    family: "Noto Sans",
    file: "NotoSans-Regular-subset.ttf",
    embedded: true,
};
pub const JAPANESE: InterfaceFont = InterfaceFont {
    family: "Noto Sans JP",
    file: "NotoSansJP-Regular-subset.ttf",
    embedded: false,
};
pub const KOREAN: InterfaceFont = InterfaceFont {
    family: "Noto Sans KR",
    file: "NotoSansKR-Regular-subset.ttf",
    embedded: false,
};
pub const TRADITIONAL_CHINESE: InterfaceFont = InterfaceFont {
    family: "Noto Sans TC",
    file: "NotoSansTC-Regular-subset.ttf",
    embedded: false,
};

/// Every face, embedded first.
pub const ALL: [InterfaceFont; 5] = [
    SIMPLIFIED_CHINESE,
    LATIN,
    JAPANESE,
    KOREAN,
    TRADITIONAL_CHINESE,
];

/// The face the interface uses for a catalog language code. Russian uses
/// Noto Sans so Latin and Cyrillic come from one design; the other Latin
/// languages keep the default face.
pub fn interface_font(language_code: &str) -> InterfaceFont {
    match language_code {
        "ja" => JAPANESE,
        "ko" => KOREAN,
        "zh-TW" => TRADITIONAL_CHINESE,
        "ru" => LATIN,
        _ => SIMPLIFIED_CHINESE,
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) mod loader {
    use super::{InterfaceFont, interface_font};
    use std::borrow::Cow;
    use std::cell::RefCell;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    #[derive(Default)]
    struct State {
        /// Fetched faces registered with the text system.
        loaded: Vec<&'static str>,
        /// Faces being fetched; a second language switch does not refetch.
        pending: Vec<&'static str>,
        /// Fetches that failed; not retried until the page reloads.
        failed: Vec<&'static str>,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State::default());
    }

    /// Point the interface at the current language's face, fetching it first
    /// when it is not embedded. Called once after launch and on every
    /// language or override change.
    pub(crate) fn sync(cx: &mut gpui::App) {
        let font = interface_font(sqlly_i18n::current_language().code());
        let ready =
            font.embedded || STATE.with(|state| state.borrow().loaded.contains(&font.family));
        if ready {
            use_family(font.family, cx);
            return;
        }
        // Keep the current face until the regional one arrives; text the
        // embedded subsets lack is drawn by the browser meanwhile.
        let start = STATE.with(|state| {
            let mut state = state.borrow_mut();
            if state.pending.contains(&font.family) || state.failed.contains(&font.family) {
                false
            } else {
                state.pending.push(font.family);
                true
            }
        });
        if start {
            cx.spawn(async move |cx| {
                let result = fetch(font).await;
                cx.update(|cx| finish(font, result, cx));
            })
            .detach();
        }
    }

    fn finish(font: InterfaceFont, result: Result<Vec<u8>, String>, cx: &mut gpui::App) {
        let registered = result.and_then(|bytes| {
            cx.text_system()
                .add_fonts(vec![Cow::Owned(bytes)])
                .map_err(|error| format!("{error:#}"))
        });
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.pending.retain(|family| *family != font.family);
            match &registered {
                Ok(()) => state.loaded.push(font.family),
                Err(_) => state.failed.push(font.family),
            }
        });
        match registered {
            // The language may have changed again while the face loaded.
            Ok(()) => sync(cx),
            Err(error) => log::warn!("could not load the {} interface font: {error}", font.family),
        }
    }

    fn use_family(family: &'static str, cx: &mut gpui::App) {
        let changed = sqlly_gpui::set_browser_interface_font_family(family);
        cx.global_mut::<gpui_component::theme::Theme>().font_family = family.into();
        if changed {
            cx.refresh_windows();
        }
    }

    async fn fetch(font: InterfaceFont) -> Result<Vec<u8>, String> {
        let window = web_sys::window().ok_or("no browser window")?;
        // Relative to the page, so it works wherever dist/ is served. The
        // version makes the long-lived static cache safe across releases.
        let url = format!("fonts/{}?v={}", font.file, env!("CARGO_PKG_VERSION"));
        let response: web_sys::Response = JsFuture::from(window.fetch_with_str(&url))
            .await
            .map_err(|error| format!("{url}: {error:?}"))?
            .dyn_into()
            .map_err(|_| format!("{url}: not a fetch response"))?;
        if !response.ok() {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        let buffer = JsFuture::from(
            response
                .array_buffer()
                .map_err(|error| format!("{url}: {error:?}"))?,
        )
        .await
        .map_err(|error| format!("{url}: {error:?}"))?;
        Ok(js_sys::Uint8Array::new(&buffer).to_vec())
    }
}
