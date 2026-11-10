//! SQLLY browser preview (in-tab demo) — not a production frontend and not
//! for connecting to real servers.
//!
//! This crate is only the wasm entry point: it boots gpui's web platform
//! backend (`gpui_web`, WebGL/WebGPU canvas rendering), registers the fonts a
//! browser cannot supply, opens the in-tab database backend, and then hands
//! control to the same [`sqlly_gpui::init_ui`] bootstrap the native `SQLLY`
//! binary runs. It is a labeled preview: CI keeps it as a compile check, and
//! it is not a hosted Cloud SKU. Platform differences live behind
//! `cfg(target_arch = "wasm32")` seams inside the application and service
//! crates:
//!
//! - queries run against in-tab SQLite (rusqlite over sqlite-wasm-rs) or the
//!   `sqlly-fakedb` demo backend — a browser cannot open TDS sockets;
//! - settings/connections persist to `localStorage`; secrets (SQL passwords,
//!   tokens, API keys) to `sessionStorage`, cleared when the tab closes;
//! - local/cloud AI, Azure/Entra sign-in, relay, the external editor, and
//!   native file dialogs report "not available in the browser preview — use
//!   the desktop app";
//! - with `?proxy=wss://…` (or `globalThis.SQLLY_PROXY_URL`) the page hands
//!   `run` a `sqlly-relay` browser-proxy URL, and the app then queries the
//!   real servers that relay serves — read-only connection list, credentials
//!   never leave the relay host.
//!
//! On native targets this crate compiles as an empty rlib so the workspace
//! `cargo check`/`cargo test` cover it on every platform.

#[cfg(target_arch = "wasm32")]
mod embedded_assets {
    //! Serve gpui-component's icon SVGs from the wasm binary itself.
    //!
    //! The upstream `gpui_kit_assets::Assets` wasm loader fetches each
    //! icon over HTTP on demand and returns an error until the async fetch
    //! lands — but nothing schedules a repaint when it does, and any
    //! serving-path/CORS/404 issue makes the icon permanently blank ("Wasm
    //! assets loading, will be available soon..." logged every frame). Icons
    //! are tiny (~400 KB for the whole set, next to a multi-MB wasm module), so
    //! embed them and answer `load` synchronously — every icon is present on
    //! the first paint, regardless of how dist/ is served.

    use std::borrow::Cow;

    use gpui::{AssetSource, Result, SharedString};
    use include_dir::{Dir, include_dir};

    // Vendored from gpui-kit-assets (its `default-icons.txt` set, under
    // assets/icons) to match the pinned gpui-component version; refresh when
    // that dependency bumps. Last refreshed for gpui-kit-assets 0.7.3, which
    // added ban, circle-alert, mic, refresh-cw and square to the default set.
    static ICONS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets/icons");

    pub struct EmbeddedAssets;

    impl AssetSource for EmbeddedAssets {
        fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
            // SQLLY's own drawn glyphs (`glyphs/<name>.svg`) come from the app
            // crate, which embeds them at compile time for every host.
            if let Some(bytes) = sqlly_gpui::glyph(path) {
                return Ok(Some(Cow::Borrowed(bytes)));
            }
            // gpui-component requests icons as "icons/<name>.svg"; everything
            // else (fonts are registered separately) is not ours to serve.
            let Some(name) = path.strip_prefix("icons/") else {
                return Ok(None);
            };
            Ok(ICONS.get_file(name).map(|f| Cow::Borrowed(f.contents())))
        }

        fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
            Ok(ICONS
                .files()
                .filter_map(|f| f.path().file_name())
                .map(|n| SharedString::from(format!("icons/{}", n.to_string_lossy())))
                .collect())
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod mac_command_key {
    //! Make ⌘ behave like Ctrl for a Mac browser.
    //!
    //! A wasm build is never `target_os = "macos"`, so every keybinding layer
    //! — the SQL editor's defaults, the app shortcuts, gpui-component's text
    //! inputs, and gpui's own `secondary` modifier — resolves to the Ctrl
    //! flavor. On a Mac that left ⌘A/⌘Z/⌘C dead and Ctrl+A/Ctrl+Z as the only
    //! working chords. Rather than re-deciding each of those at runtime, a
    //! capture-phase listener on `window` (which runs before gpui's listeners
    //! on the canvas and its hidden input) rewrites the event so ⌘ reads as
    //! Ctrl. Physical Ctrl still reads as Ctrl, so Ctrl+Space and friends keep
    //! working.
    //!
    //! ⌘V is left alone: gpui has no synchronous clipboard read on the web, so
    //! the browser's native `paste` event (which ⌘V fires when gpui doesn't
    //! claim the keystroke) is the path that pastes without a permission
    //! prompt and carries images.

    use js_sys::{Object, Reflect};
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};

    /// Events whose modifier flags gpui reads. Wheel is excluded on purpose:
    /// browsers report trackpad pinch as a Ctrl+wheel, and ⌘-scroll must not
    /// turn into a pinch.
    const FOLDED_EVENTS: &[&str] = &[
        "keydown",
        "keyup",
        "pointerdown",
        "pointerup",
        "pointermove",
    ];

    pub fn install() {
        let Some(window) = web_sys::window() else {
            return;
        };
        if !is_mac(&window.navigator()) {
            return;
        }
        let fold = Closure::<dyn FnMut(JsValue)>::new(|event: JsValue| fold_command(&event));
        for name in FOLDED_EVENTS {
            window
                .add_event_listener_with_callback_and_bool(
                    name,
                    fold.as_ref().unchecked_ref(),
                    true,
                )
                .ok();
        }
        // Lives for the page, like the app itself.
        fold.forget();
    }

    fn is_mac(navigator: &web_sys::Navigator) -> bool {
        // Same probe gpui_web uses for its own `is_mac`; `platform` is
        // deprecated but `userAgentData` is not available everywhere yet.
        #[allow(deprecated)]
        let platform = navigator.platform().unwrap_or_default();
        platform.contains("Mac") || navigator.user_agent().unwrap_or_default().contains("Mac")
    }

    fn fold_command(event: &JsValue) {
        if !flag(event, "metaKey") || is_paste_chord(event) {
            return;
        }
        // Own data properties shadow the prototype getters gpui_web's
        // web-sys bindings read; the event object is discarded after dispatch.
        set_flag(event, "ctrlKey", true);
        set_flag(event, "metaKey", false);
    }

    fn is_paste_chord(event: &JsValue) -> bool {
        Reflect::get(event, &"key".into())
            .ok()
            .and_then(|key| key.as_string())
            .is_some_and(|key| key.eq_ignore_ascii_case("v"))
    }

    fn flag(event: &JsValue, name: &str) -> bool {
        Reflect::get(event, &name.into())
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn set_flag(event: &JsValue, name: &str, value: bool) {
        let descriptor = Object::new();
        Reflect::set(&descriptor, &"value".into(), &JsValue::from_bool(value)).ok();
        Object::define_property(event.unchecked_ref::<Object>(), &name.into(), &descriptor);
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::{borrow::Cow, cell::RefCell, rc::Rc};

    use gpui::{AppCell, Application};
    use wasm_bindgen::prelude::*;

    use super::embedded_assets::EmbeddedAssets;

    thread_local! {
        static WASM_APP: RefCell<Option<Rc<AppCell>>> = const { RefCell::new(None) };
    }

    fn with_app(update: impl FnOnce(&mut gpui::App) -> Result<(), String>) -> Result<(), JsValue> {
        WASM_APP.with(|slot| {
            let slot = slot.borrow();
            let app = slot
                .as_ref()
                .ok_or_else(|| JsValue::from_str("SQLLY is not ready"))?;
            // A host command can arrive while gpui is inside an update (a
            // re-entrant JS callback during a frame). Report that instead of
            // panicking on the RefCell, so the host can retry.
            let mut app = app
                .try_borrow_mut()
                .map_err(|_| JsValue::from_str("SQLLY is busy; try again"))?;
            update(&mut app).map_err(|error| JsValue::from_str(&error))
        })
    }

    /// Settles the promise `run` returns. gpui-web's `Platform::run` returns
    /// before graphics initialization finishes and calls the launch callback
    /// later, from a spawned task — and never calls it when graphics fail
    /// (it drops it instead). The host must not treat the app as ready, or
    /// push translation state, until `init_ui` has run: `init_ui` resets the
    /// language and clears overrides, wiping anything applied earlier. So
    /// the launch callback resolves this signal after `init_ui`, and
    /// dropping it unresolved (graphics failed) rejects.
    struct LaunchSignal {
        resolve: js_sys::Function,
        reject: js_sys::Function,
        settled: bool,
    }

    impl LaunchSignal {
        fn resolve(mut self) {
            self.settled = true;
            self.resolve.call0(&JsValue::UNDEFINED).ok();
        }
    }

    impl Drop for LaunchSignal {
        fn drop(&mut self) {
            if !self.settled {
                self.reject
                    .call1(
                        &JsValue::UNDEFINED,
                        &js_sys::Error::new(
                            "SQLLY could not start: browser graphics are unavailable",
                        ),
                    )
                    .ok();
            }
        }
    }

    #[wasm_bindgen]
    pub fn set_language(language: String) -> Result<(), JsValue> {
        with_app(|cx| sqlly_gpui::apply_host_language(&language, cx))
    }

    /// Browser chrome resolves through the same active catalog as the workbench.
    #[wasm_bindgen]
    pub fn translation_message(key: String, arguments_json: String) -> Result<String, JsValue> {
        let arguments: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&arguments_json)
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
        if arguments.is_empty() {
            Ok(sqlly_gpui::translation_text(&key))
        } else {
            let arguments = arguments
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            Ok(sqlly_gpui::translation_format(&key, &arguments))
        }
    }

    /// Replace the live-preview overrides with the manager's catalog. Keys this
    /// build does not know (server/app version skew) or whose placeholders
    /// differ from English are skipped rather than failing the whole push;
    /// their keys are returned so the host acknowledgement can report them.
    /// Malformed JSON still throws and leaves the previous overrides active.
    #[wasm_bindgen]
    pub fn replace_translation_overrides(overrides_json: String) -> Result<Vec<String>, JsValue> {
        let mut skipped = Vec::new();
        with_app(|cx| {
            skipped = sqlly_gpui::apply_host_overrides(&overrides_json, cx)?;
            Ok(())
        })?;
        Ok(skipped)
    }

    #[wasm_bindgen]
    pub fn clear_translation_overrides() -> Result<(), JsValue> {
        with_app(sqlly_gpui::clear_host_overrides)
    }

    /// Entry point called from the host page after the wasm module loads.
    ///
    /// Returns a promise that resolves once the application has launched
    /// (after `init_ui`), and rejects if the browser's graphics could not be
    /// initialized and the app never launched. The host gates readiness —
    /// and every host translation command — on it.
    ///
    /// `northwind_db` is the bytes of the bundled Northwind SQLite sample
    /// (fetched same-origin by the host page and staged into `dist/` by
    /// `build-browser.sh`). It is empty when the sample could not be fetched,
    /// in which case the app falls back to the emulated demo database.
    ///
    /// `proxy_url` (with an optional `proxy_token`) points the app at a
    /// `sqlly-relay` browser proxy: the app then runs against the servers
    /// and databases that relay serves, read-only, instead of the in-tab
    /// samples. `None` keeps the standalone preview.
    #[wasm_bindgen]
    pub fn run(
        northwind_db: Vec<u8>,
        proxy_url: Option<String>,
        proxy_token: Option<String>,
    ) -> Result<js_sys::Promise, JsValue> {
        console_error_panic_hook::set_once();
        console_log::init_with_level(log::Level::Info).ok();
        tracing_wasm::set_as_global_default();

        // Must precede `init_ui`: the app reads it while it builds its first
        // window and decides whether to seed the sample connections.
        sqlly_gpui::configure_browser_proxy(
            proxy_url.filter(|u| !u.trim().is_empty()),
            proxy_token.filter(|t| !t.trim().is_empty()),
        );

        super::mac_command_key::install();
        gpui_platform::web_init();
        let app = {
            let app = gpui_platform::single_threaded_web();

            // Keep the application alive for the page's lifetime: `run` never
            // returns control to a blocking main loop on wasm, so the Rc that
            // owns the app state is deliberately leaked (same workaround as
            // gpui-component's story-web). A second retained handle supports
            // synchronous host translation commands on this wasm thread.
            struct WasmApplication(Rc<AppCell>);
            let wasm_app = unsafe { std::mem::transmute::<Application, WasmApplication>(app) };
            WASM_APP.with(|slot| *slot.borrow_mut() = Some(wasm_app.0.clone()));
            std::mem::forget(wasm_app.0.clone());
            unsafe { std::mem::transmute::<WasmApplication, Application>(wasm_app) }
        };

        // Open the database the explorer's seeded sample connection points at.
        // Prefer the bundled Northwind SQLite sample handed in by the host
        // page; if it is absent (fetch failed, or serving without the build's
        // staged file), fall back to the emulated demo database, which works
        // offline and includes stored procedures. Either way a backend is
        // open before the first connection is made, so queries answer
        // immediately.
        let opened_sqlite = if northwind_db.is_empty() {
            false
        } else {
            match sqlly_services::browser::open_sqlite_database(&northwind_db) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!(
                        "could not open the bundled Northwind sample ({error}); \
                         falling back to the demo database"
                    );
                    false
                }
            }
        };
        if !opened_sqlite {
            sqlly_services::browser::open_demo_database();
        }

        let services = sqlly_services::Services::new();

        let mut signal = None;
        let launched = js_sys::Promise::new(&mut |resolve, reject| {
            signal = Some(LaunchSignal {
                resolve,
                reject,
                settled: false,
            });
        });
        let signal = signal.expect("Promise executors run synchronously");

        // Icons (gpui-component's SVG set) are embedded in this wasm binary and
        // served synchronously — see EmbeddedAssets. This avoids the upstream
        // HTTP-fetch loader whose async, serving-path-dependent behavior left
        // most icons blank in the browser.
        app.with_assets(EmbeddedAssets)
            .run(move |cx: &mut gpui::App| {
                // The browser has no system fonts. `init_ui` registers Fira
                // Code (SQL/results); the UI sans face must also be embedded,
                // and the component theme pointed at it before first paint.
                // "Noto Sans" (Latin subset) is additionally registered because
                // it is on gpui's fallback stack: text drawn with the default
                // style (`.SystemUIFont`, e.g. tooltips) or any unregistered
                // family resolves to it instead of panicking — on web no
                // platform translation of `.SystemUIFont` exists and no system
                // fallback can ever load.
                //
                // Both subsets are augmented with the geometric/arrow/keyboard
                // symbol glyphs the UI draws as literal text (tree twisties,
                // close crosses, plan warnings, keybinding hints, …). Native
                // gets these from the OS font stack; the browser has none, so
                // any uncovered codepoint renders as tofu. `fonts/build-symbols.py`
                // merges them in — re-run it if the subsets are regenerated.
                let fonts: Vec<Cow<'static, [u8]>> = vec![
                    Cow::Borrowed(include_bytes!("../fonts/NotoSansSC-Regular-subset.ttf")),
                    Cow::Borrowed(include_bytes!("../fonts/NotoSans-Regular-subset.ttf")),
                ];
                cx.text_system()
                    .add_fonts(fonts)
                    .expect("failed to register embedded UI font");

                // The real application — identical bootstrap to the desktop.
                sqlly_gpui::init_ui(services.clone(), cx);

                cx.global_mut::<gpui_component::theme::Theme>().font_family = "Noto Sans SC".into();

                // Only now is the host's translation state safe to apply.
                signal.resolve();
            });

        Ok(launched)
    }
}
