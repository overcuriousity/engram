//! Packs the Chrome extension into `assets/extension/chrome.zip`.
//!
//! Built from source at compile time so a deployment always serves the build
//! that matches it, and so there is no second artifact to publish or forget.
//! Written into `assets/` rather than `OUT_DIR` because `rust-embed` embeds
//! that directory; the file is generated and is gitignored.
//!
//! The Firefox XPI is not built here. It must be AMO-signed, which is a
//! network round trip that does not belong in `cargo build`, so it is signed
//! once per release and committed. See `extension/README.md`.

use std::io::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=extension/shared");
    println!("cargo:rerun-if-changed=extension/chrome/manifest.json");

    let out = Path::new("assets/extension");
    std::fs::create_dir_all(out).expect("assets/extension");
    let file = std::fs::File::create(out.join("chrome.zip")).expect("chrome.zip");
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let manifest = std::fs::read("extension/chrome/manifest.json").expect("chrome manifest");
    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(&manifest).unwrap();

    // Flat `shared/` inside the package, matching the paths the manifest names.
    // Read from `extension/shared` rather than from the copy `pack.sh` makes,
    // so a stale copy cannot end up in a release.
    let mut names: Vec<String> = std::fs::read_dir("extension/shared")
        .expect("extension/shared")
        .filter_map(|e| e.ok())
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
