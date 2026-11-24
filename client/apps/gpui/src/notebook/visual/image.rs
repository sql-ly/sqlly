//! Report SVG shown in the app: rasterized with the report's own bundled
//! fonts (so the in-app chart matches the exported one exactly) and cached
//! by what was drawn, its size and the theme. The browser build, which has
//! no rasterizer, hands gpui the SVG itself.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use sqlly_services::report::theme::{self, ReportTheme};

use crate::theme::Theme;

/// Whether the app is drawing dark, which picks the report theme.
pub fn is_dark() -> bool {
    crate::theme::relative_luminance(Theme::bg()) < 0.35
}

pub fn report_theme() -> &'static ReportTheme {
    if is_dark() {
        &theme::DARK
    } else {
        &theme::LIGHT
    }
}

/// A drawn image and the key it was drawn for.
#[derive(Clone, Default)]
pub struct CachedImage {
    key: u64,
    image: Option<Arc<gpui::Image>>,
    pub error: Option<String>,
}

impl CachedImage {
    /// The image for `key`, drawing it with `draw` (SVG markup) only when
    /// the key changed.
    pub fn get(
        &mut self,
        key: impl Hash,
        scale: f32,
        draw: impl FnOnce() -> String,
    ) -> Option<Arc<gpui::Image>> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        is_dark().hash(&mut h);
        ((scale * 100.0) as u32).hash(&mut h);
        let key = h.finish();
        if key != self.key || self.image.is_none() && self.error.is_none() {
            self.key = key;
            match svg_image(&draw(), scale) {
                Ok(image) => {
                    self.image = Some(image);
                    self.error = None;
                }
                Err(error) => {
                    self.image = None;
                    self.error = Some(error);
                }
            }
        }
        self.image.clone()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn svg_image(svg: &str, scale: f32) -> Result<Arc<gpui::Image>, String> {
    let png = sqlly_services::report::png::rasterize(svg, scale.clamp(1.0, 3.0))?;
    Ok(Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png,
    )))
}

#[cfg(target_arch = "wasm32")]
fn svg_image(svg: &str, _scale: f32) -> Result<Arc<gpui::Image>, String> {
    Ok(Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Svg,
        svg.as_bytes().to_vec(),
    )))
}
