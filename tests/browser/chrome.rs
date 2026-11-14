//! Where the browser tests find a headless Chrome.

use std::path::PathBuf;
use std::process::Command;

pub fn chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ENGRAM_CHROME") {
        return Some(PathBuf::from(p));
    }
    if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(&home).join(".cache/ms-playwright");
        if let Ok(entries) = std::fs::read_dir(&cache) {
            let mut found: Vec<(u64, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let bin = e
                        .path()
                        .join("chrome-headless-shell-linux64/chrome-headless-shell");
                    // Playwright's build number from the directory suffix (`chromium_headless_shell-1148`), else zero.
                    let build = e
                        .file_name()
                        .to_string_lossy()
                        .rsplit('-')
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    bin.exists().then_some((build, bin))
                })
                .collect();
            // Newest build wins, by number: sorting paths puts `-999` after `-1148`.
            found.sort();
            if let Some((_, p)) = found.pop() {
                return Some(p);
            }
        }
    }
    for name in ["chrome-headless-shell", "chromium", "google-chrome"] {
        if let Ok(out) = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name}"))
            .output()
            && out.status.success()
        {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}
