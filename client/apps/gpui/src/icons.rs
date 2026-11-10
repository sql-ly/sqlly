//! Raster brand artwork (PNG logos) that the UI draws as images rather than
//! themeable glyphs. Every monochrome, themeable icon now lives in the
//! generated [`crate::icon_catalog`] (see `assets/icons/manifest.toml`); this
//! module is only the full-colour logos the titlebar and dialogs embed.

use std::sync::{Arc, OnceLock};

use gpui::{RenderImage, Styled, px};
use gpui_component::Icon;

use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

/// Hugeicons disclosure chevron: down when expanded, right when collapsed.
pub fn disclosure_chevron(expanded: bool) -> Icon {
    Icon::empty()
        .path(if expanded {
            AppIcon::ChevronDown.path()
        } else {
            AppIcon::ChevronRight.path()
        })
        .size(px(Theme::scaled(10.0)))
}

/// Padlock, sized for a result name's header.
pub fn lock() -> Icon {
    Icon::empty()
        .path(AppIcon::Lock.path())
        .size(px(Theme::scaled(11.0)))
}

/// Down chevron for menus and selects.
pub fn chevron_down() -> Icon {
    Icon::empty()
        .path(AppIcon::ChevronDown.path())
        .size(px(Theme::toolbar_icon_size()))
}

const ENTRA_ID_PNG: &[u8] = include_bytes!("../resources/icons/entra-id@2x.png");
/// 32×32 pixel-art pig; drawn at 16pt so retina gets a true 2× mark.
const SQLLY_PNG: &[u8] = include_bytes!("../resources/icons/sqlly@2x.png");
/// 192×192 pixel-art pig for empty-query and other large marks.
const SQLLY_LARGE_PNG: &[u8] = include_bytes!("../resources/icons/sqlly-192.png");

fn load() -> Option<Arc<RenderImage>> {
    let dynamic = image::load_from_memory(ENTRA_ID_PNG).ok()?;
    let rgba = dynamic.to_rgba8();
    let frame = image::Frame::new(rgba);
    Some(Arc::new(RenderImage::new([frame])))
}

pub fn entra_id_icon() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    ICON.get_or_init(load).clone()
}

fn load_brand_png(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let dynamic = image::load_from_memory(bytes).ok()?;
    // GPUI's RenderImage samples the buffer as BGRA — swap the red/blue
    // channels so the artwork keeps its true colors.
    let mut rgba = dynamic.to_rgba8();
    for px in rgba.pixels_mut() {
        px.0.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    Some(Arc::new(RenderImage::new([frame])))
}

/// The 16pt title-bar pig (32px source).
pub fn sqlly_icon() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    ICON.get_or_init(|| load_brand_png(SQLLY_PNG)).clone()
}

/// The large pig used when the query area is empty.
pub fn sqlly_icon_large() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    ICON.get_or_init(|| load_brand_png(SQLLY_LARGE_PNG)).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entra_id_png_loads() {
        let icon = entra_id_icon();
        assert!(icon.is_some(), "Entra ID PNG should decode successfully");
    }

    #[test]
    fn entra_id_png_is_cached() {
        let a = entra_id_icon();
        let b = entra_id_icon();
        assert!(
            a.as_ref()
                .map(|a| Arc::ptr_eq(a, b.as_ref().unwrap()))
                .unwrap_or(false),
            "entra_id_icon should return the same Arc on repeated calls"
        );
    }

    #[test]
    fn sqlly_titlebar_png_is_32px() {
        let img = image::load_from_memory(SQLLY_PNG).expect("title-bar PNG should decode");
        assert_eq!(img.width(), 32, "title-bar mark is 32px (@2x for 16pt)");
        assert_eq!(img.height(), 32);
        assert!(
            sqlly_icon().is_some(),
            "title-bar pig should decode as a GPU image"
        );
    }

    #[test]
    fn sqlly_large_png_is_192px() {
        let img = image::load_from_memory(SQLLY_LARGE_PNG).expect("large PNG should decode");
        assert_eq!(img.width(), 192);
        assert_eq!(img.height(), 192);
        assert!(
            sqlly_icon_large().is_some(),
            "large pig should decode as a GPU image"
        );
    }

    #[test]
    fn sqlly_icon_is_cached() {
        let a = sqlly_icon();
        let b = sqlly_icon();
        assert!(
            a.as_ref()
                .map(|a| Arc::ptr_eq(a, b.as_ref().unwrap()))
                .unwrap_or(false),
            "sqlly_icon should return the same Arc on repeated calls"
        );
    }
}
