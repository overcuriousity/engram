//! What happens to an uploaded image before anything is stored: sniff, decode, read EXIF, preview.

use crate::error::{Error, Result};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

#[derive(Debug)]
pub struct PreparedImage {
    /// Sniffed from the bytes, not taken from the client.
    pub mime: &'static str,
    /// After the EXIF orientation is applied.
    pub width: u32,
    pub height: u32,
    pub preview_jpeg: Vec<u8>,
    /// `{}` when the file carries none.
    pub exif: serde_json::Value,
}

/// High enough that small print in a photographed page survives.
const PREVIEW_QUALITY: u8 = 85;

/// Phone sensors top out near 9 000 px; a file claiming more costs width × height × 4 before any
/// check.
pub const MAX_IMAGE_EDGE: u32 = 10_000;
pub const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// Decodes run on ungated `spawn_blocking`, so without this a few concurrent uploads reach the OOM
/// killer. Not configurable: it keeps the process alive, it is not a throughput setting.
pub const MAX_CONCURRENT_DECODES: usize = 2;

pub fn prepare(bytes: &[u8], preview_edge: u32) -> Result<PreparedImage> {
    let format = image::guess_format(bytes).map_err(|_| {
        Error::Validation("that upload is not a supported image (JPEG, PNG or WebP)".into())
    })?;
    let mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::WebP => "image/webp",
        other => {
            return Err(Error::Validation(format!(
                "that image is {} — only JPEG, PNG and WebP are accepted",
                other
                    .extensions_str()
                    .first()
                    .copied()
                    .unwrap_or("of an unsupported format")
            )));
        }
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => Error::Validation(format!(
            "that image is too large to read — at most {MAX_IMAGE_EDGE} pixels on a side"
        )),
        e => Error::Validation(format!("that image could not be decoded: {e}")),
    })?;

    let exif = read_exif(bytes);
    let exif_json = exif
        .as_ref()
        .map(exif_to_json)
        .unwrap_or_else(|| serde_json::json!({}));
    let orientation = exif_json["orientation"]
        .as_u64()
        .and_then(|o| image::metadata::Orientation::from_exif(o as u8))
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut img = decoded;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    let preview_jpeg = encode_preview(&img, preview_edge)?;

    Ok(PreparedImage {
        mime,
        width,
        height,
        preview_jpeg,
        exif: exif_json,
    })
}

fn read_exif(bytes: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
}

fn encode_preview(img: &DynamicImage, edge: u32) -> Result<Vec<u8>> {
    // Before the resize: a wide kernel mixes RGB under transparent pixels into a dark fringe.
    let img = &DynamicImage::ImageRgb8(flatten_onto_white(img));
    let scaled = if img.width().max(img.height()) > edge {
        // Lanczos, not `thumbnail`: its integer algorithm produces moiré on photographed text.
        img.resize(edge, edge, image::imageops::FilterType::Lanczos3)
    } else {
        img.clone()
    };
    let rgb = scaled;
    let mut out = Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, PREVIEW_QUALITY);
    rgb.write_with_encoder(enc)
        .map_err(|e| Error::Internal(format!("preview encoding failed: {e}")))?;
    Ok(out.into_inner())
}

/// White, because transparency in a capture is nearly always the page under dark text.
fn flatten_onto_white(img: &DynamicImage) -> image::RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p.0[3] as u32;
        let over = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([over(p.0[0]), over(p.0[1]), over(p.0[2])]));
    }
    out
}

/// The `file` namespace of a corpus's metadata.
pub fn file_facts(name: Option<&str>, size: usize, img: &PreparedImage) -> serde_json::Value {
    let mut v = serde_json::json!({
        "size": size,
        "mime": img.mime,
        "width": img.width,
        "height": img.height,
    });
    if let Some(n) = name {
        v["name"] = serde_json::Value::String(n.to_string());
    }
    v
}

/// The `exif` namespace: the named facts, then every other tag under `tags`.
pub fn exif_to_json(exif: &exif::Exif) -> serde_json::Value {
    use exif::{In, Tag};
    let mut out = serde_json::Map::new();

    let ascii = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(v) => v
                    .first()
                    .map(|b| String::from_utf8_lossy(b).trim().to_string()),
                _ => None,
            })
    };

    if let Some(dt) = ascii(Tag::DateTimeOriginal) {
        let mut iso = dt.replacen(':', "-", 2).replacen(' ', "T", 1);
        if let Some(off) = ascii(Tag::OffsetTimeOriginal) {
            iso.push_str(&off);
        }
        out.insert("taken_at".into(), iso.into());
    }
    let camera: Vec<String> = [ascii(Tag::Make), ascii(Tag::Model)]
        .into_iter()
        .flatten()
        .collect();
    if !camera.is_empty() {
        out.insert("camera".into(), camera.join(" ").into());
    }
    if let Some(o) = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
    {
        out.insert("orientation".into(), o.into());
    }
    if let Some(gps) = gps_json(exif) {
        out.insert("gps".into(), gps);
    }

    // The original is not stored, so any tag dropped here is gone for good.
    let mut tags = serde_json::Map::new();
    for f in exif.fields() {
        if f.ifd_num != In::PRIMARY {
            continue;
        }
        let name = f.tag.to_string();
        if matches!(f.tag, Tag::MakerNote | Tag::UserComment) || name.starts_with("Tag(") {
            continue;
        }
        // ASCII fields display quoted; the quotes are the formatter's, not the file's.
        let value: String = match &f.value {
            exif::Value::Ascii(v) => v
                .first()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_default(),
            _ => f.display_value().with_unit(exif).to_string(),
        };
        let value: String = value.chars().take(200).collect();
        tags.insert(name, value.into());
    }
    if !tags.is_empty() {
        out.insert("tags".into(), tags.into());
    }

    serde_json::Value::Object(out)
}

fn gps_json(exif: &exif::Exif) -> Option<serde_json::Value> {
    use exif::{In, Tag};
    let reference = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(v) => v
                    .first()
                    .map(|b| String::from_utf8_lossy(b).trim().to_string()),
                _ => None,
            })
    };
    let dms = |tag: Tag, r: Tag, neg: &str| -> Option<f64> {
        let f = exif.get_field(tag, In::PRIMARY)?;
        let exif::Value::Rational(v) = &f.value else {
            return None;
        };
        if v.len() < 3 {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let sign = match reference(r) {
            Some(s) if s == neg => -1.0,
            _ => 1.0,
        };
        Some(deg * sign)
    };
    let lat = dms(Tag::GPSLatitude, Tag::GPSLatitudeRef, "S")?;
    let lon = dms(Tag::GPSLongitude, Tag::GPSLongitudeRef, "W")?;
    let mut g = serde_json::json!({ "lat": lat, "lon": lon });
    if let Some(f) = exif.get_field(Tag::GPSAltitude, In::PRIMARY)
        && let exif::Value::Rational(v) = &f.value
        && let Some(a) = v.first()
    {
        g["alt"] = serde_json::json!(a.to_f64());
    }
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};
    use std::io::Cursor;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn a_hairline_survives_the_downscale_wherever_it_falls() {
        // A preview edge of 512 gives a factor of 3.9, where point sampling drops columns.
        const W: u32 = 2000;
        const PITCH: u32 = 20;
        let src = ImageBuffer::from_fn(W, 200, |x, _| {
            if x % PITCH < 2 {
                Rgb([0u8, 0, 0])
            } else {
                Rgb([255u8, 255, 255])
            }
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(src)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();

        let prepared = prepare(&out.into_inner(), 512).unwrap();
        let preview = image::load_from_memory(&prepared.preview_jpeg)
            .unwrap()
            .to_luma8();

        let scale = preview.width() as f32 / W as f32;
        let column_min = |x: u32| -> u8 {
            (0..preview.height())
                .map(|y| preview.get_pixel(x, y).0[0])
                .min()
                .unwrap()
        };
        let lost: Vec<u32> = (0..W / PITCH)
            .filter(|i| {
                let centre = (((i * PITCH + 1) as f32) * scale).round() as u32;
                let lo = centre.saturating_sub(1);
                let hi = (centre + 1).min(preview.width() - 1);
                (lo..=hi).all(|x| column_min(x) > 200)
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{} of {} hairlines left no trace in the preview: {:?}",
            lost.len(),
            W / PITCH,
            lost
        );
    }

    #[test]
    fn transparency_is_flattened_onto_white_rather_than_onto_black() {
        let src = ImageBuffer::from_fn(64, 64, |x, y| {
            if (20..44).contains(&x) && (20..44).contains(&y) {
                image::Rgba([10u8, 10, 10, 255])
            } else {
                image::Rgba([0u8, 0, 0, 0])
            }
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(src)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();

        let prepared = prepare(&out.into_inner(), 2048).unwrap();
        let preview = image::load_from_memory(&prepared.preview_jpeg)
            .unwrap()
            .to_luma8();

        assert!(
            preview.get_pixel(2, 2).0[0] > 240,
            "the transparent ground must come out white, not black; it was {}",
            preview.get_pixel(2, 2).0[0]
        );
        assert!(
            preview.get_pixel(32, 32).0[0] < 40,
            "and the opaque mark must still be dark"
        );
    }

    #[test]
    fn an_image_declaring_absurd_dimensions_is_refused_before_it_is_decoded() {
        // IHDR patched to claim 11000x11000, under the decoder's own allocation ceiling, so only
        // our limit refuses it.
        let img = ImageBuffer::from_fn(1, 1, |_, _| Rgb([1u8, 2, 3]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        let mut png = out.into_inner();
        // Signature (8) + length (4) + "IHDR" (4) + width (4) + height (4) ...
        png[16..20].copy_from_slice(&11_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&11_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());

        let e = prepare(&png, 2048).unwrap_err();
        assert!(matches!(e, Error::Validation(_)), "{e}");
        assert!(e.to_string().contains("large"), "{e}");
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, _| Rgb([(x % 256) as u8, 0, 0]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, _| Rgb([0, (x % 256) as u8, 0]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn jpeg_with_exif(w: u32, h: u32, fields: &[exif::Field]) -> Vec<u8> {
        let mut writer = exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        let mut blob = Cursor::new(Vec::new());
        writer.write(&mut blob, false).unwrap();
        let blob = blob.into_inner();
        let mut app1 = Vec::new();
        app1.extend_from_slice(&[0xFF, 0xE1]);
        let len = (blob.len() + 6 + 2) as u16;
        app1.extend_from_slice(&len.to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&blob);
        let plain = jpeg(w, h);
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&app1);
        out.extend_from_slice(&plain[2..]);
        out
    }

    fn ascii(tag: exif::Tag, s: &str) -> exif::Field {
        exif::Field {
            tag,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![s.as_bytes().to_vec()]),
        }
    }

    #[test]
    fn a_png_is_decoded_measured_and_previewed_as_jpeg() {
        let p = prepare(&png(300, 100), 2048).unwrap();
        assert_eq!(p.mime, "image/png");
        assert_eq!((p.width, p.height), (300, 100));
        assert_eq!(p.exif, serde_json::json!({}));
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!(
            image::guess_format(&p.preview_jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert_eq!((prev.width(), prev.height()), (300, 100));
    }

    #[test]
    fn a_large_image_is_downscaled_to_the_edge_keeping_its_ratio() {
        let p = prepare(&jpeg(4000, 2000), 1000).unwrap();
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!((prev.width(), prev.height()), (1000, 500));
        assert_eq!((p.width, p.height), (4000, 2000));
    }

    #[test]
    fn exif_orientation_is_applied_to_the_preview_and_recorded() {
        let orient = exif::Field {
            tag: exif::Tag::Orientation,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Short(vec![6]), // rotate 90° CW
        };
        let p = prepare(&jpeg_with_exif(400, 200, &[orient]), 2048).unwrap();
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!((prev.width(), prev.height()), (200, 400));
        assert_eq!(
            (p.width, p.height),
            (200, 400),
            "dimensions are as displayed"
        );
        assert_eq!(p.exif["orientation"], 6);
    }

    #[test]
    fn exif_facts_are_mapped_and_the_rest_kept_as_tags() {
        let fields = vec![
            ascii(exif::Tag::DateTimeOriginal, "2026:08:09 14:12:03"),
            ascii(exif::Tag::Make, "Apple"),
            ascii(exif::Tag::Model, "iPhone 15"),
            ascii(exif::Tag::Software, "17.5"),
        ];
        let p = prepare(&jpeg_with_exif(64, 64, &fields), 2048).unwrap();
        assert_eq!(p.exif["taken_at"], "2026-08-09T14:12:03");
        assert_eq!(p.exif["camera"], "Apple iPhone 15");
        assert_eq!(p.exif["tags"]["Software"], "17.5");
        assert!(p.exif.get("gps").is_none());
    }

    #[test]
    fn gps_is_converted_to_decimal_degrees() {
        let dms = |d: u32, m: u32, s: u32| {
            exif::Value::Rational(vec![
                exif::Rational { num: d, denom: 1 },
                exif::Rational { num: m, denom: 1 },
                exif::Rational {
                    num: s * 100,
                    denom: 100,
                },
            ])
        };
        let f = |tag, value| exif::Field {
            tag,
            ifd_num: exif::In::PRIMARY,
            value,
        };
        let fields = vec![
            f(exif::Tag::GPSLatitude, dms(48, 12, 30)),
            ascii(exif::Tag::GPSLatitudeRef, "N"),
            f(exif::Tag::GPSLongitude, dms(16, 22, 0)),
            ascii(exif::Tag::GPSLongitudeRef, "W"),
            f(
                exif::Tag::GPSAltitude,
                exif::Value::Rational(vec![exif::Rational {
                    num: 1710,
                    denom: 10,
                }]),
            ),
        ];
        let p = prepare(&jpeg_with_exif(64, 64, &fields), 2048).unwrap();
        let g = &p.exif["gps"];
        assert!((g["lat"].as_f64().unwrap() - 48.208333).abs() < 1e-4, "{g}");
        assert!((g["lon"].as_f64().unwrap() + 16.366667).abs() < 1e-4, "{g}");
        assert!((g["alt"].as_f64().unwrap() - 171.0).abs() < 1e-6);
    }

    #[test]
    fn junk_and_unsupported_formats_are_refused_with_the_reason() {
        let e = prepare(b"not an image at all", 2048).unwrap_err();
        assert!(matches!(e, crate::error::Error::Validation(_)));
        assert!(e.to_string().contains("not a supported image"), "{e}");

        let e = prepare(b"GIF89a\x01\x00\x01\x00\x80\x00\x00", 2048).unwrap_err();
        assert!(e.to_string().contains("gif"), "{e}");
    }

    #[test]
    fn file_facts_carry_name_size_and_dimensions() {
        let p = prepare(&png(30, 10), 2048).unwrap();
        let f = file_facts(Some("IMG_1.png"), 1234, &p);
        assert_eq!(
            f,
            serde_json::json!({"name": "IMG_1.png", "size": 1234, "mime": "image/png", "width": 30, "height": 10})
        );
        assert!(file_facts(None, 1, &p).get("name").is_none());
    }
}
