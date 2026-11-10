//! Native-resolution screenshot of one app window, for Send Feedback.
//!
//! GPUI has no production screenshot API (its `render_to_image` is
//! test-support only), so capture goes through the OS:
//!
//! - **macOS**: the window's `NSWindow` number (via the raw window handle)
//!   is handed to `CGWindowListCreateImage` with best-resolution imaging, so
//!   a Retina window comes back at its full backing resolution. An app may
//!   always image its own windows — no Screen Recording permission prompt.
//! - **Windows / Linux**: not implemented yet; [`capture_target`] reports the
//!   screenshot as unavailable and the dialog sends the message on its own.
//! - **wasm**: this module is not compiled (Send Feedback is native-only).
//!
//! Capture is split in two so the pixel copy stays off the UI thread:
//! [`capture_target`] reads the window identity on the main thread, and
//! [`capture`] (which is `Send`-safe) does the imaging and conversion.

use image::RgbaImage;

/// A captured window image at native resolution.
pub struct Screenshot {
    pub image: RgbaImage,
    /// Backing pixels per window point (2.0 on Retina).
    pub scale_factor: f32,
}

/// Which window to image, resolved on the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureTarget {
    /// A macOS window number (`NSWindow.windowNumber` / `CGWindowID`).
    #[cfg(target_os = "macos")]
    MacWindow {
        window_number: u32,
        scale_factor: f32,
    },
    /// Capture is not available here; the string says why (shown to the user).
    Unavailable(String),
}

/// Resolve the capture target for `window`.
pub fn capture_target(window: &gpui::Window) -> CaptureTarget {
    #[cfg(target_os = "macos")]
    {
        match macos::window_number(window) {
            Some(window_number) => CaptureTarget::MacWindow {
                window_number,
                scale_factor: window.scale_factor(),
            },
            None => CaptureTarget::Unavailable(crate::localization::text(
                "feedback.screenshot.window_unidentified",
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        CaptureTarget::Unavailable(crate::localization::text(
            "feedback.screenshot.platform_unsupported",
        ))
    }
}

/// Image the target. Safe to run on a background thread.
pub fn capture(target: &CaptureTarget) -> Result<Screenshot, String> {
    match target {
        #[cfg(target_os = "macos")]
        CaptureTarget::MacWindow {
            window_number,
            scale_factor,
        } => macos::capture_window(*window_number).map(|image| Screenshot {
            image,
            scale_factor: *scale_factor,
        }),
        CaptureTarget::Unavailable(reason) => Err(reason.clone()),
    }
}

/// Memory order of a 32-bit-per-pixel source buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum PixelLayout {
    Bgra,
    Rgba,
    Argb,
    Abgr,
}

/// Map a `CGBitmapInfo` to the memory byte order of its 32-bit pixels.
/// `None` for layouts a window capture never produces (float, 16-bit, …).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn layout_from_bitmap_info(info: u32) -> Option<PixelLayout> {
    const ALPHA_MASK: u32 = 0x1F;
    const BYTE_ORDER_MASK: u32 = 0x7000;
    const ORDER_DEFAULT: u32 = 0;
    const ORDER_32_BIG: u32 = 4 << 12;
    const ORDER_32_LITTLE: u32 = 2 << 12;
    const FLOAT: u32 = 1 << 8;
    if info & FLOAT != 0 {
        return None;
    }
    // Alpha position within the (big-endian) 32-bit word.
    let alpha_first = match info & ALPHA_MASK {
        2 | 4 | 6 => true,  // PremultipliedFirst, First, NoneSkipFirst
        1 | 3 | 5 => false, // PremultipliedLast, Last, NoneSkipLast
        _ => return None,
    };
    match (info & BYTE_ORDER_MASK, alpha_first) {
        (ORDER_DEFAULT | ORDER_32_BIG, true) => Some(PixelLayout::Argb),
        (ORDER_DEFAULT | ORDER_32_BIG, false) => Some(PixelLayout::Rgba),
        (ORDER_32_LITTLE, true) => Some(PixelLayout::Bgra),
        (ORDER_32_LITTLE, false) => Some(PixelLayout::Abgr),
        _ => None,
    }
}

/// Copy a 32-bit-per-pixel buffer (rows `stride` bytes apart, possibly
/// padded) into a straight-alpha RGBA image, un-premultiplying when
/// `premultiplied`. Returns `None` when the buffer is too small.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn convert_32bpp(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    layout: PixelLayout,
    premultiplied: bool,
) -> Option<RgbaImage> {
    let row_bytes = width as usize * 4;
    if stride < row_bytes || data.len() < stride * (height as usize).saturating_sub(1) + row_bytes {
        return None;
    }
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let row = &data[y * stride..y * stride + row_bytes];
        for px in row.as_chunks::<4>().0 {
            let (r, g, b, a) = match layout {
                PixelLayout::Bgra => (px[2], px[1], px[0], px[3]),
                PixelLayout::Rgba => (px[0], px[1], px[2], px[3]),
                PixelLayout::Argb => (px[1], px[2], px[3], px[0]),
                PixelLayout::Abgr => (px[3], px[2], px[1], px[0]),
            };
            let (r, g, b) = if premultiplied && a != 0 && a != 255 {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                (un(r), un(g), un(b))
            } else {
                (r, g, b)
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    RgbaImage::from_raw(width, height, out)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;

    use image::RgbaImage;
    use raw_window_handle::RawWindowHandle;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    type CGImageRef = *const c_void;
    type CFDataRef = *const c_void;

    const LIST_OPTION_INCLUDING_WINDOW: u32 = 1 << 3;
    const IMAGE_BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
    const IMAGE_BEST_RESOLUTION: u32 = 1 << 3;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCreateImage(
            screen_bounds: CGRect,
            list_option: u32,
            window_id: u32,
            image_option: u32,
        ) -> CGImageRef;
        fn CGImageGetWidth(image: CGImageRef) -> usize;
        fn CGImageGetHeight(image: CGImageRef) -> usize;
        fn CGImageGetBitsPerComponent(image: CGImageRef) -> usize;
        fn CGImageGetBitsPerPixel(image: CGImageRef) -> usize;
        fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
        fn CGImageGetBitmapInfo(image: CGImageRef) -> u32;
        fn CGImageGetDataProvider(image: CGImageRef) -> *const c_void;
        fn CGDataProviderCopyData(provider: *const c_void) -> CFDataRef;
        fn CGImageRelease(image: CGImageRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
        fn CFDataGetLength(data: CFDataRef) -> isize;
        fn CFRelease(cf: *const c_void);
    }

    /// `NSWindow.windowNumber` for the gpui window, via its content view.
    pub(super) fn window_number(window: &gpui::Window) -> Option<u32> {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let handle = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        let view = appkit.ns_view.as_ptr() as *mut AnyObject;
        // SAFETY: `ns_view` is the live NSView gpui hosts its content in; we
        // only send it and its window read-only messages on the main thread.
        let number: isize = unsafe {
            let ns_window: *mut AnyObject = msg_send![view, window];
            if ns_window.is_null() {
                return None;
            }
            msg_send![ns_window, windowNumber]
        };
        u32::try_from(number).ok().filter(|&n| n != 0)
    }

    /// Image one window at its backing resolution, without the frame shadow.
    pub(super) fn capture_window(window_number: u32) -> Result<RgbaImage, String> {
        // CGRectNull: "the minimum rectangle enclosing the listed windows".
        let null_rect = CGRect {
            origin: CGPoint {
                x: f64::INFINITY,
                y: f64::INFINITY,
            },
            size: CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        // SAFETY: plain CoreGraphics C calls; every created object is
        // released on every path below.
        unsafe {
            let image = CGWindowListCreateImage(
                null_rect,
                LIST_OPTION_INCLUDING_WINDOW,
                window_number,
                IMAGE_BOUNDS_IGNORE_FRAMING | IMAGE_BEST_RESOLUTION,
            );
            if image.is_null() {
                return Err(crate::localization::text(
                    "feedback.screenshot.capture_failed",
                ));
            }
            let result = (|| {
                let width = CGImageGetWidth(image) as u32;
                let height = CGImageGetHeight(image) as u32;
                if width == 0 || height == 0 {
                    return Err(crate::localization::text(
                        "feedback.screenshot.capture_empty",
                    ));
                }
                if CGImageGetBitsPerComponent(image) != 8 || CGImageGetBitsPerPixel(image) != 32 {
                    return Err(crate::localization::text(
                        "feedback.screenshot.pixel_format_unsupported",
                    ));
                }
                let info = CGImageGetBitmapInfo(image);
                let layout = super::layout_from_bitmap_info(info).ok_or_else(|| {
                    crate::localization::text("feedback.screenshot.pixel_layout_unsupported")
                })?;
                let premultiplied = matches!(info & 0x1F, 1 | 2);
                let stride = CGImageGetBytesPerRow(image);
                let provider = CGImageGetDataProvider(image);
                if provider.is_null() {
                    return Err(crate::localization::text(
                        "feedback.screenshot.pixel_data_missing",
                    ));
                }
                let data = CGDataProviderCopyData(provider);
                if data.is_null() {
                    return Err(crate::localization::text(
                        "feedback.screenshot.pixel_data_missing",
                    ));
                }
                let len = CFDataGetLength(data).max(0) as usize;
                let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), len);
                let converted =
                    super::convert_32bpp(bytes, width, height, stride, layout, premultiplied);
                CFRelease(data);
                converted.ok_or_else(|| {
                    crate::localization::text("feedback.screenshot.capture_truncated")
                })
            })();
            CGImageRelease(image);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_info_maps_to_memory_order() {
        // The usual window capture: 32-bit little-endian, premultiplied first.
        assert_eq!(
            layout_from_bitmap_info((2 << 12) | 2),
            Some(PixelLayout::Bgra)
        );
        assert_eq!(
            layout_from_bitmap_info((2 << 12) | 6),
            Some(PixelLayout::Bgra)
        );
        assert_eq!(layout_from_bitmap_info(1), Some(PixelLayout::Rgba));
        assert_eq!(layout_from_bitmap_info(5), Some(PixelLayout::Rgba));
        assert_eq!(
            layout_from_bitmap_info((4 << 12) | 2),
            Some(PixelLayout::Argb)
        );
        assert_eq!(
            layout_from_bitmap_info((2 << 12) | 1),
            Some(PixelLayout::Abgr)
        );
        assert_eq!(layout_from_bitmap_info(0), None); // no alpha channel info
        assert_eq!(layout_from_bitmap_info((1 << 8) | 1), None); // float
    }

    #[test]
    fn converts_bgra_rows_with_padding() {
        // 2×2 image, 12-byte stride (4 bytes of row padding).
        let data = [
            10, 20, 30, 255, 40, 50, 60, 255, 0, 0, 0, 0, //
            70, 80, 90, 255, 1, 2, 3, 255, 0, 0, 0, 0,
        ];
        let img = convert_32bpp(&data, 2, 2, 12, PixelLayout::Bgra, false).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [60, 50, 40, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [90, 80, 70, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [3, 2, 1, 255]);
    }

    #[test]
    fn un_premultiplies_translucent_pixels() {
        // Premultiplied half-transparent white → straight white at alpha 128.
        let data = [128, 128, 128, 128];
        let img = convert_32bpp(&data, 1, 1, 4, PixelLayout::Rgba, true).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 128]);
        let argb = [255, 1, 2, 3];
        let img = convert_32bpp(&argb, 1, 1, 4, PixelLayout::Argb, true).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [1, 2, 3, 255]);
    }

    #[test]
    fn short_buffers_are_rejected() {
        assert!(convert_32bpp(&[0; 7], 2, 1, 8, PixelLayout::Rgba, false).is_none());
        assert!(convert_32bpp(&[0; 16], 2, 2, 4, PixelLayout::Rgba, false).is_none());
    }
}
