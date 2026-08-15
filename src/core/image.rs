//! What happens to an uploaded image before anything is stored: sniff the
//! format, decode it, read its EXIF, and derive the one preview the vision
//! model is shown and the UI displays. Pure functions; no I/O.

use crate::error::{Error, Result};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

#[derive(Debug)]
pub struct PreparedImage {
    /// Of the original, from its bytes rather than from what the client said.
    pub mime: &'static str,
    /// As displayed: after the EXIF orientation is applied.
    pub width: u32,
    pub height: u32,
    /// Orientation applied, longest edge at most `preview_edge`, JPEG.
    pub preview_jpeg: Vec<u8>,
    /// See `exif_to_json`. `{}` when the file carries none.
    pub exif: serde_json::Value,
}

/// JPEG quality of the preview: high enough that small print in a photographed
/// page survives, well below the original's size.
const PREVIEW_QUALITY: u8 = 85;

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
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| Error::Validation(format!("that image could not be decoded: {e}")))?;

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
    let scaled = if img.width().max(img.height()) > edge {
        img.thumbnail(edge, edge)
    } else {
        img.clone()
    };
    // JPEG has no alpha; a PNG with transparency is flattened rather than refused.
    let rgb = DynamicImage::ImageRgb8(scaled.to_rgb8());
    let mut out = Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, PREVIEW_QUALITY);
    rgb.write_with_encoder(enc)
        .map_err(|e| Error::Internal(format!("preview encoding failed: {e}")))?;
    Ok(out.into_inner())
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

/// The `exif` namespace: the handful of facts worth naming, then every other
/// tag under `tags` by name so nothing the file carries is thrown away.
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
        // "2026:08:09 14:12:03" → "2026-08-09T14:12:03", offset appended when the
        // file says one.
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

    let mut tags = serde_json::Map::new();
    for f in exif.fields() {
        if f.ifd_num != In::PRIMARY {
            continue;
        }
        let name = f.tag.to_string();
        // Already named above, or binary noise nobody reads back.
        if matches!(f.tag, Tag::MakerNote | Tag::UserComment) || name.starts_with("Tag(") {
            continue;
        }
        // ASCII fields display quoted; the quotes are the formatter's, not
        // the file's.
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

    /// A JPEG carrying the given EXIF fields in an APP1 segment, spliced in
    /// right after SOI — which is where every camera puts it.
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
        // Not upscaled: smaller than the edge stays its own size.
        assert_eq!((prev.width(), prev.height()), (300, 100));
    }

    #[test]
    fn a_large_image_is_downscaled_to_the_edge_keeping_its_ratio() {
        let p = prepare(&jpeg(4000, 2000), 1000).unwrap();
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!((prev.width(), prev.height()), (1000, 500));
        // The recorded size is the original's.
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

        // A GIF header sniffs as an image but is not one of the three.
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
