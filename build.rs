//! Packs the browser extension into `assets/` (embedded by `rust-embed`, gitignored) and
//! prefers a committed AMO-signed XPI, since release Firefox will not keep an unsigned one.

use std::io::Write;
use std::path::Path;

const SIGNED: &str = "extension/firefox-signed.xpi";

type Res = Result<(), Box<dyn std::error::Error>>;

fn at<E: std::fmt::Display>(
    what: impl std::fmt::Display,
) -> impl FnOnce(E) -> Box<dyn std::error::Error> {
    move |e| format!("{what}: {e}").into()
}

fn main() -> Res {
    #[cfg(feature = "contained")]
    build_whisper()?;
    build_stylesheet()?;

    println!("cargo:rerun-if-changed=extension/shared");
    println!("cargo:rerun-if-changed={TOKENS}");
    println!("cargo:rerun-if-changed=extension/chrome/manifest.json");
    println!("cargo:rerun-if-changed=extension/firefox/manifest.json");
    // Watched only where it exists: Cargo treats a missing watched path as always changed.
    if std::path::Path::new(SIGNED).exists() {
        println!("cargo:rerun-if-changed={SIGNED}");
    }

    stamp_build()?;

    let out = Path::new("assets/extension");
    std::fs::create_dir_all(out).map_err(at("assets/extension"))?;

    pack("extension/chrome/manifest.json", &out.join("chrome.zip"))?;

    // The marker is what the install page reads to tell the operator which of
    // the two they are downloading, since both arrive under the same name.
    let marker = out.join("firefox.signed");
    if Path::new(SIGNED).exists() {
        std::fs::copy(SIGNED, out.join("firefox.xpi")).map_err(at(SIGNED))?;
        std::fs::write(&marker, b"").map_err(at("the signed marker"))?;
    } else {
        pack("extension/firefox/manifest.json", &out.join("firefox.xpi"))?;
        // A stale marker would promise one-click install of an XPI Firefox refuses.
        let _ = std::fs::remove_file(&marker);
    }
    Ok(())
}

/// Concatenated in filename order: tokens must arrive before the rules that name them.
fn build_stylesheet() -> Res {
    println!("cargo:rerun-if-changed=assets/css");
    let dir = Path::new("assets/css");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(at("assets/css"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".css"))
        .collect();
    // Sorted, so a build is reproducible.
    names.sort();

    let mut out = String::new();
    for name in &names {
        let body =
            std::fs::read_to_string(dir.join(name)).map_err(at(format!("assets/css/{name}")))?;
        out.push_str("/* ===== ");
        out.push_str(name);
        out.push_str(" ===== */\n");
        out.push_str(&body);
        out.push('\n');
    }

    // Written only when the bytes differ, so an unchanged sheet keeps its mtime.
    let dest = Path::new("assets/app.css");
    if std::fs::read_to_string(dest).ok().as_deref() != Some(out.as_str()) {
        std::fs::write(dest, &out).map_err(at("assets/app.css"))?;
    }
    Ok(())
}

/// `ENGRAM_VERSION` is `<semver>+<sha>`: build metadata an older binary's comparison ignores.
/// Only a `.git` here counts, so a tarball inside another repository takes no commit of its own.
fn stamp_build() -> Res {
    println!("cargo:rerun-if-env-changed=ENGRAM_GIT_SHA");
    let sha = std::env::var("ENGRAM_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_sha)
        .unwrap_or_else(|| "unknown".into());
    let pkg = std::env::var("CARGO_PKG_VERSION").map_err(at("CARGO_PKG_VERSION"))?;
    let version = if sha == "unknown" {
        pkg
    } else {
        format!("{pkg}+{sha}")
    };
    println!("cargo:rustc-env=ENGRAM_GIT_SHA={sha}");
    println!("cargo:rustc-env=ENGRAM_VERSION={version}");
    Ok(())
}

fn git_sha() -> Option<String> {
    if !Path::new(".git").exists() {
        return None;
    }
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").args(args).output().ok()?;
        out.status.success().then_some(())?;
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    // `--git-path`: in a worktree `.git` is a file and branch refs live in the common directory.
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(r);
    }
    for w in watch {
        if let Some(p) = git(&["rev-parse", "--git-path", &w])
            && Path::new(&p).exists()
        {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    git(&["rev-parse", "--short=12", "HEAD"])
}

const TOKENS: &str = "assets/css/00-tokens.css";
const PANEL_FONTS: [&str; 2] = ["atkinson-next-400.woff2", "atkinson-next-500.woff2"];

fn pack(manifest_path: &str, out: &Path) -> Res {
    let file = std::fs::File::create(out).map_err(at(out.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let manifest = std::fs::read(manifest_path).map_err(at(manifest_path))?;
    zip.start_file("manifest.json", opts)?;
    zip.write_all(&manifest)?;

    // Read from `extension/shared`, not the copy `pack.sh` makes, so a stale copy cannot ship.
    let mut names: Vec<String> = std::fs::read_dir("extension/shared")
        .map_err(at("extension/shared"))?
        .filter_map(|e| e.ok())
        // A subdirectory would otherwise fail the build with a bare `Is a directory`.
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // Sorted, so the archive is reproducible.
    names.sort();
    for name in names {
        let mut body =
            std::fs::read(Path::new("extension/shared").join(&name)).map_err(at(&name))?;
        // The panel takes its palette from the app's own token file.
        if name == "panel.css" {
            let mut css = std::fs::read(TOKENS).map_err(at(TOKENS))?;
            css.push(b'\n');
            css.append(&mut body);
            body = css;
        }
        zip.start_file(format!("shared/{name}"), opts)?;
        zip.write_all(&body)?;
    }
    for font in PANEL_FONTS {
        let body = std::fs::read(Path::new("assets/fonts").join(font))
            .map_err(at(format!("assets/fonts/{font}")))?;
        zip.start_file(format!("shared/{font}"), opts)?;
        zip.write_all(&body)?;
    }
    zip.finish()?;
    Ok(())
}

/// Compiled against the ggml `llama-cpp-sys-2` already built (`DEP_LLAMA_GGML_CMAKE_DIR`), so the
/// program has one ggml; see `vendor/whisper.cpp/README.md` for the pins.
#[cfg(feature = "contained")]
fn build_whisper() -> Res {
    let vendor = Path::new("vendor/whisper.cpp");
    println!("cargo:rerun-if-changed={}", vendor.display());

    let cmake_dir = std::env::var("DEP_LLAMA_GGML_CMAKE_DIR").map_err(|_| {
        "llama-cpp-sys-2 did not say where its ggml is (DEP_LLAMA_GGML_CMAKE_DIR); \
         whisper.cpp is compiled against that ggml and no other"
    })?;
    // <out>/lib/cmake or <out>/lib64/cmake; the headers are installed beside.
    let ggml_include = Path::new(&cmake_dir).join("../../include");
    if !ggml_include.join("ggml.h").is_file() {
        return Err(format!(
            "no ggml.h under {}: the layout llama-cpp-sys-2 installs has changed",
            ggml_include.display()
        )
        .into());
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(vendor.join("include"))
        .include(vendor.join("src"))
        .include(vendor)
        .include(&ggml_include)
        .define("WHISPER_VERSION", "\"1.9.2\"")
        // Vendored and unmodified: its warnings are its authors' to read.
        .warnings(false)
        .file(vendor.join("src/whisper.cpp"))
        .file(vendor.join("engram_whisper.cpp"));
    // llama links the C++ runtime statically on Android; asking again adds libc++_shared.so to the APK.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        build.cpp_link_stdlib(None);
    }
    build.compile("engram_whisper");
    Ok(())
}
