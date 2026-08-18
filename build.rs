//! Packs the browser extension into `assets/extension/`.
//!
//! Built from source at compile time so a deployment always serves the build
//! that matches it, and so there is no second artifact to publish or forget.
//! Written into `assets/` rather than `OUT_DIR` because `rust-embed` embeds
//! that directory; the files are generated and are gitignored.
//!
//! Firefox is the same package under a different name and a different
//! manifest. The XPI produced here is unsigned, which release Firefox will not
//! install permanently — it loads through `about:debugging` and lasts until
//! the browser restarts. An AMO-signed build is the way to make it stick, and
//! signing is a network round trip that does not belong in `cargo build`, so a
//! signed package is committed to `extension/firefox-signed.xpi` and preferred
//! here when it exists. See `extension/README.md`.

use std::io::Write;
use std::path::Path;

/// The signed package, when a release has been through AMO.
const SIGNED: &str = "extension/firefox-signed.xpi";

fn main() {
    println!("cargo:rerun-if-changed=extension/shared");
    println!("cargo:rerun-if-changed=extension/chrome/manifest.json");
    println!("cargo:rerun-if-changed=extension/firefox/manifest.json");
    println!("cargo:rerun-if-changed={SIGNED}");

    stamp_assets();

    let out = Path::new("assets/extension");
    std::fs::create_dir_all(out).expect("assets/extension");

    pack("extension/chrome/manifest.json", &out.join("chrome.zip"));

    // The marker is what the install page reads to tell the operator which of
    // the two they are downloading, since both arrive under the same name.
    let marker = out.join("firefox.signed");
    if Path::new(SIGNED).exists() {
        std::fs::copy(SIGNED, out.join("firefox.xpi")).expect("copy the signed xpi");
        std::fs::write(&marker, b"").expect("write the signed marker");
    } else {
        pack("extension/firefox/manifest.json", &out.join("firefox.xpi"));
        // Removed rather than left behind: a marker surviving from a build
        // that had the signed package would have the page promise one-click
        // install of an XPI Firefox refuses.
        let _ = std::fs::remove_file(&marker);
    }
}

/// A stamp derived from the assets the pages reference by name.
///
/// `/assets/app.js` and `/assets/app.css` are served with a year-long
/// `max-age` under a URL that never changes, so a browser that has been here
/// before keeps its copy across an upgrade. That was cosmetic while the pages
/// worked without JavaScript. It is not any more: the ask page is driven
/// entirely by `app.js`, so a server serving a fresh `ask.html` to a browser
/// holding last year's script gets a form that submits nothing at all —
/// silently, per browser, and with nothing the operator can do about it from
/// the server side. The service worker does not help; it returns early for
/// anything that is not a navigation, so a script never passes through it.
///
/// So the URL moves when the bytes do. Content-derived rather than a build
/// timestamp, because a rebuild that changed nothing must not throw away a
/// cache that is still correct. FNV-1a rather than a cryptographic hash: this
/// only has to differ when the file differs, and it costs no dependency.
fn stamp_assets() {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for name in ["assets/app.js", "assets/app.css"] {
        println!("cargo:rerun-if-changed={name}");
        for b in std::fs::read(name).unwrap_or_else(|e| panic!("{name}: {e}")) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=ASSET_STAMP={h:x}");
}

/// Zip one browser's manifest together with the shared sources.
fn pack(manifest_path: &str, out: &Path) {
    let file = std::fs::File::create(out).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let manifest = std::fs::read(manifest_path).unwrap_or_else(|e| panic!("{manifest_path}: {e}"));
    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(&manifest).unwrap();

    // Flat `shared/` inside the package, matching the paths the manifest names.
    // Read from `extension/shared` rather than from the copy `pack.sh` makes,
    // so a stale copy cannot end up in a release.
    let mut names: Vec<String> = std::fs::read_dir("extension/shared")
        .expect("extension/shared")
        .filter_map(|e| e.ok())
        // Files only. A subdirectory added here would otherwise be read as a
        // file and fail the build with a bare `Is a directory`.
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // Sorted, so the same sources produce the same archive twice running.
    names.sort();
    for name in names {
        let body = std::fs::read(Path::new("extension/shared").join(&name)).unwrap();
        zip.start_file(format!("shared/{name}"), opts).unwrap();
        zip.write_all(&body).unwrap();
    }
    zip.finish().unwrap();
}
