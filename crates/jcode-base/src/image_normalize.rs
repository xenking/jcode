//! Normalize image bytes into a format every vision provider accepts.
//!
//! Anthropic, OpenAI and Gemini all accept `image/png`, `image/jpeg`,
//! `image/gif` and `image/webp`. Anything else (BMP, ICO, TIFF, HEIC, ...)
//! is rejected with a 400. Once such a block is persisted in a session it is
//! replayed on every later request, so a single unsupported image used to brick
//! the whole conversation (#1712).
//!
//! Every place that turns bytes into an image block (the `read` tool,
//! drag-and-drop paste, generated images) and the outbound request chokepoint
//! route through [`normalize_image_bytes`]. The format is detected from magic
//! bytes, never from the file extension. Decodable non-provider formats are
//! re-encoded to PNG. Everything else is refused with a human-readable reason
//! so callers can fall back to text.

/// Media types every vision-capable provider jcode talks to accepts.
pub const PROVIDER_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// True when `media_type` is one of [`PROVIDER_IMAGE_MEDIA_TYPES`].
pub fn is_provider_media_type(media_type: &str) -> bool {
    PROVIDER_IMAGE_MEDIA_TYPES
        .iter()
        .any(|accepted| accepted.eq_ignore_ascii_case(media_type))
}

/// Detect a provider-accepted media type from the leading magic bytes.
/// Returns `None` for every other format.
pub fn sniff_provider_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Name of a recognised non-provider image format, for conversion and for
/// user-facing explanations.
fn sniff_other_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 2 && &bytes[0..2] == b"BM" {
        return Some("BMP");
    }
    if bytes.len() >= 4 && bytes[0..4] == [0, 0, 1, 0] {
        return Some("ICO");
    }
    if bytes.len() >= 4 && (&bytes[0..4] == b"II*\0" || &bytes[0..4] == b"MM\0*") {
        return Some("TIFF");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some("AVIF");
        }
        if [
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1",
        ]
        .iter()
        .any(|b| brand == *b)
        {
            return Some("HEIC");
        }
    }
    let head = &bytes[..bytes.len().min(256)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("<svg") || (head.starts_with("<?xml") && head.contains("<svg")) {
        return Some("SVG");
    }
    None
}

/// An image ready to be attached to a provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderImage {
    /// One of [`PROVIDER_IMAGE_MEDIA_TYPES`], matching the bytes.
    pub media_type: &'static str,
    pub data: Vec<u8>,
    /// Source format name when the bytes were re-encoded (for example "BMP").
    pub converted_from: Option<&'static str>,
}

/// Turn arbitrary image bytes into a provider-safe image.
///
/// - PNG/JPEG/GIF/WebP pass through untouched, labelled by their magic bytes.
/// - BMP/ICO/TIFF are decoded and re-encoded as PNG.
/// - Anything else (HEIC, AVIF, SVG, corrupt data) returns `Err` with a short
///   explanation suitable for showing to the model or the user.
pub fn normalize_image_bytes(data: Vec<u8>) -> Result<ProviderImage, String> {
    if let Some(media_type) = sniff_provider_media_type(&data) {
        return Ok(ProviderImage {
            media_type,
            data,
            converted_from: None,
        });
    }
    match sniff_other_format(&data) {
        Some(format @ ("BMP" | "ICO" | "TIFF")) => match reencode_png(&data) {
            Some(png) => Ok(ProviderImage {
                media_type: "image/png",
                data: png,
                converted_from: Some(format),
            }),
            None => Err(format!(
                "{format} data could not be decoded, so it cannot be sent to the model"
            )),
        },
        Some(format) => Err(format!(
            "{format} is not a format the model can view directly; convert it to PNG or JPEG and read the result"
        )),
        None => Err("unrecognised or corrupt image data, so it cannot be sent to the model".into()),
    }
}

/// Longest edge of an image a tool hands to the model.
///
/// A Retina screenshot is 3456x2234 and 2-11 MB as PNG. Every later request of
/// the session carries it again, so 25 of them filled the 24 MiB request image
/// budget, the oldest were cut, and the provider rebuilt a 465K-token prompt
/// cache (2026-11-19). The model reads an image of this size just as well.
pub const MODEL_IMAGE_MAX_EDGE: u32 = 1568;
/// Encoded size an image handed to the model is brought under.
pub const MODEL_IMAGE_MAX_BYTES: usize = 500 * 1024;

/// An image sized for the model, and what it was scaled from if it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedImage {
    pub image: ProviderImage,
    /// `(original, sent)` pixel sizes when the image was scaled down.
    pub scaled: Option<((u32, u32), (u32, u32))>,
}

/// Bring `image` within [`MODEL_IMAGE_MAX_EDGE`] and [`MODEL_IMAGE_MAX_BYTES`].
///
/// An image already inside both bounds is returned byte for byte. Otherwise it
/// is scaled down and stored as PNG or JPEG, whichever is smaller. Animated
/// GIFs and anything that fails to decode are left alone: the outbound clamp
/// still enforces the provider's hard limits on those.
pub fn fit_image_for_model(image: ProviderImage) -> FittedImage {
    let untouched = |image| FittedImage {
        image,
        scaled: None,
    };
    if image.media_type == "image/gif" {
        return untouched(image);
    }
    let Ok(reader) =
        image::ImageReader::new(std::io::Cursor::new(&image.data)).with_guessed_format()
    else {
        return untouched(image);
    };
    let Ok((width, height)) = reader.into_dimensions() else {
        return untouched(image);
    };
    let over_edge = width.max(height) > MODEL_IMAGE_MAX_EDGE;
    if !over_edge && image.data.len() <= MODEL_IMAGE_MAX_BYTES {
        return untouched(image);
    }
    let Ok(decoded) = image::load_from_memory(&image.data) else {
        return untouched(image);
    };
    let mut scale = (MODEL_IMAGE_MAX_EDGE as f64 / width.max(height) as f64).min(1.0);
    let mut best: Option<(Vec<u8>, &'static str, (u32, u32))> = None;
    for _ in 0..5 {
        let target = (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        );
        let resized = if target == (width, height) {
            decoded.clone()
        } else {
            decoded.resize_exact(target.0, target.1, image::imageops::FilterType::CatmullRom)
        };
        let Some((data, media_type)) = encode_smallest(&resized) else {
            break;
        };
        let len = data.len();
        best = Some((data, media_type, target));
        if len <= MODEL_IMAGE_MAX_BYTES || target == (1, 1) {
            break;
        }
        // Encoded size follows pixel area, so shrink each edge by the root of
        // the overshoot.
        scale *= (MODEL_IMAGE_MAX_BYTES as f64 / len as f64)
            .sqrt()
            .clamp(0.5, 0.9);
    }
    match best {
        Some((data, media_type, size)) if over_edge || data.len() < image.data.len() => {
            FittedImage {
                image: ProviderImage {
                    media_type,
                    data,
                    converted_from: image.converted_from,
                },
                scaled: (size != (width, height)).then_some(((width, height), size)),
            }
        }
        _ => untouched(image),
    }
}

/// [`fit_image_for_model`] for an image already held as base64, as tool
/// results are. Anything that is not a decodable image comes back unchanged.
pub fn fit_base64_image_for_model(media_type: String, data: String) -> (String, String) {
    use base64::Engine as _;
    let engine = &base64::engine::general_purpose::STANDARD;
    // 4 base64 chars carry 3 bytes: skip the decode when the image is small
    // enough that only its pixel size could still be over.
    if data.len() / 4 * 3 <= MODEL_IMAGE_MAX_BYTES / 8 {
        return (media_type, data);
    }
    let Ok(bytes) = engine.decode(data.trim()) else {
        return (media_type, data);
    };
    let Ok(image) = normalize_image_bytes(bytes) else {
        return (media_type, data);
    };
    let converted = image.converted_from.is_some();
    let fitted = fit_image_for_model(image);
    if fitted.scaled.is_none() && !converted && fitted.image.media_type == media_type {
        return (media_type, data);
    }
    (
        fitted.image.media_type.to_string(),
        engine.encode(&fitted.image.data),
    )
}

/// PNG or JPEG, whichever is smaller. JPEG cannot hold transparency, so an
/// image with any is kept as PNG.
fn encode_smallest(img: &image::DynamicImage) -> Option<(Vec<u8>, &'static str)> {
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).ok()?;
    let png = png.into_inner();
    let transparent =
        img.color().has_alpha() && img.to_rgba8().pixels().any(|pixel| pixel.0[3] != 255);
    if transparent {
        return Some((png, "image/png"));
    }
    let mut jpeg = Vec::new();
    let rgb = img.to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&rgb)
        .ok()?;
    Some(if png.len() <= jpeg.len() {
        (png, "image/png")
    } else {
        (jpeg, "image/jpeg")
    })
}

fn reencode_png(data: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(data).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Retina-sized screenshot: flat panels, a gradient and fine detail.
    fn screenshot_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            let fine = if (x / 3 + y / 7) % 11 == 0 { 255 } else { 20 };
            image::Rgba([(x * 255 / width) as u8, fine, ((x ^ y) & 0xff) as u8, 255])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn a_retina_screenshot_is_scaled_to_the_model_bounds() {
        let png = screenshot_png(3456, 2234);
        assert!(
            png.len() > MODEL_IMAGE_MAX_BYTES,
            "fixture is a large image"
        );
        let fitted = fit_image_for_model(normalize_image_bytes(png).unwrap());
        let decoded = image::load_from_memory(&fitted.image.data).expect("fitted image decodes");
        assert!(decoded.width().max(decoded.height()) <= MODEL_IMAGE_MAX_EDGE);
        assert!(fitted.image.data.len() <= MODEL_IMAGE_MAX_BYTES);
        assert_eq!(
            sniff_provider_media_type(&fitted.image.data),
            Some(fitted.image.media_type),
            "the media type matches the bytes"
        );
        let (from, to) = fitted.scaled.expect("the scaling is reported");
        assert_eq!(from, (3456, 2234));
        assert_eq!(to, (decoded.width(), decoded.height()));
    }

    #[test]
    fn an_image_within_the_model_bounds_is_not_reencoded() {
        let png = screenshot_png(400, 300);
        let fitted = fit_image_for_model(normalize_image_bytes(png.clone()).unwrap());
        assert_eq!(fitted.image.data, png);
        assert_eq!(fitted.scaled, None);
    }

    /// Not committed evidence, a measurement: `JCODE_FIT_SAMPLE=<image> cargo
    /// test fit_sample -- --ignored --nocapture` prints what a real file becomes.
    #[test]
    #[ignore = "needs JCODE_FIT_SAMPLE"]
    fn fit_sample() {
        let path = std::env::var("JCODE_FIT_SAMPLE").expect("JCODE_FIT_SAMPLE");
        let bytes = std::fs::read(&path).unwrap();
        let before = bytes.len();
        let fitted = fit_image_for_model(normalize_image_bytes(bytes).unwrap());
        println!(
            "{path}: {before} -> {} bytes, {} {:?}",
            fitted.image.data.len(),
            fitted.image.media_type,
            fitted.scaled
        );
    }

    #[test]
    fn transparency_survives_scaling() {
        let img = image::RgbaImage::from_fn(2400, 1200, |x, y| {
            image::Rgba([
                x as u8,
                y as u8,
                (x ^ y) as u8,
                if x < 1200 { 0 } else { 255 },
            ])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        let fitted = fit_image_for_model(normalize_image_bytes(out.into_inner()).unwrap());
        assert_eq!(fitted.image.media_type, "image/png");
        let decoded = image::load_from_memory(&fitted.image.data)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.get_pixel(10, 10).0[3], 0);
    }

    fn tiny_bmp() -> Vec<u8> {
        // 2x2 24-bit BMP, rows padded to 4 bytes.
        let row: [u8; 8] = [0, 0, 255, 0, 255, 0, 0, 0];
        let pixels: Vec<u8> = row.iter().chain(row.iter()).copied().collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&2i32.to_le_bytes());
        out.extend_from_slice(&2i32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&pixels);
        out
    }

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn encode_as(format: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([9, 8, 7, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn provider_formats_pass_through_untouched() {
        let png = tiny_png();
        let out = normalize_image_bytes(png.clone()).unwrap();
        assert_eq!(out.media_type, "image/png");
        assert_eq!(out.data, png);
        assert_eq!(out.converted_from, None);
    }

    #[test]
    fn bmp_ico_and_tiff_are_converted_to_png() {
        for (bytes, name) in [
            (tiny_bmp(), "BMP"),
            (encode_as(image::ImageFormat::Ico), "ICO"),
            (encode_as(image::ImageFormat::Tiff), "TIFF"),
        ] {
            let out = normalize_image_bytes(bytes).unwrap();
            assert_eq!(out.media_type, "image/png", "{name}");
            assert_eq!(out.converted_from, Some(name));
            assert_eq!(sniff_provider_media_type(&out.data), Some("image/png"));
            image::load_from_memory(&out.data).expect("converted PNG decodes");
        }
    }

    #[test]
    fn undecodable_formats_are_refused_with_a_reason() {
        let mut heic = vec![0, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0; 16]);
        let err = normalize_image_bytes(heic).unwrap_err();
        assert!(err.contains("HEIC"), "{err}");

        let err = normalize_image_bytes(b"<svg xmlns='x'></svg>".to_vec()).unwrap_err();
        assert!(err.contains("SVG"), "{err}");

        let err = normalize_image_bytes(b"definitely not an image".to_vec()).unwrap_err();
        assert!(err.contains("unrecognised"), "{err}");

        let err = normalize_image_bytes(b"BMgarbage".to_vec()).unwrap_err();
        assert!(err.contains("BMP"), "{err}");
    }
}
