//! PNG charts rasterized from the report's own SVG with resvg. Only the
//! bundled report fonts are loaded (never the system's), and images are
//! never fetched from paths or URLs, so a render is the same on every
//! machine.

use std::sync::{Arc, OnceLock};

use resvg::tiny_skia;
use resvg::usvg;

use super::pdf::fonts::{MONO_FONT, SANS_FONT};

/// Largest raster side in pixels.
pub const MAX_SIDE: u32 = 8192;

fn font_db() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(SANS_FONT.to_vec());
        db.load_font_data(MONO_FONT.to_vec());
        db.set_sans_serif_family("Noto Sans");
        db.set_monospace_family("JetBrains Mono");
        Arc::new(db)
    })
    .clone()
}

/// The SVG markup as PNG bytes at `scale` × its own size.
pub fn rasterize(svg: &str, scale: f32) -> Result<Vec<u8>, String> {
    let mut options = usvg::Options {
        font_family: "Noto Sans".into(),
        resources_dir: None,
        ..Default::default()
    };
    options.fontdb = font_db();
    options.image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(|_, _| None),
    };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| e.to_string())?;
    let scale = scale.clamp(0.25, 8.0);
    let size = tree.size();
    let width = (size.width() * scale).round() as u32;
    let height = (size.height() * scale).round() as u32;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("image size {width}x{height} is out of range"));
    }
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("could not allocate the image")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizes_at_the_requested_density() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"20\"><rect width=\"40\" height=\"20\" fill=\"#1a5fb4\"/><text x=\"2\" y=\"14\" font-size=\"10\">Hi</text></svg>";
        let png = rasterize(svg, 2.0).unwrap();
        assert_eq!(crate::report::text::png_dimensions(&png), Some((80, 40)));
        assert_eq!(rasterize(svg, 2.0).unwrap(), png, "deterministic");
    }

    #[test]
    fn external_images_are_not_loaded() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"4\" height=\"4\"><image xlink:href=\"/etc/passwd\" width=\"4\" height=\"4\"/></svg>";
        assert!(rasterize(svg, 1.0).is_ok());
    }
}
