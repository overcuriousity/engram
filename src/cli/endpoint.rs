//! Which engram this client is talking to, and with what credential.

use crate::error::{Error, Result};

pub struct Endpoint {
    /// No trailing slash, so joining is concatenation and never guesswork.
    pub url: String,
    pub token: String,
}

/// Never derived: a derived one prints the bearer token, and this struct ends
/// up inside a `Result` that a panic message, a log line or a test failure will
/// happily render. The address is the useful half and the credential is not.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url)
            .field("token", &"engram_…")
            .finish()
    }
}

impl Endpoint {
    /// The full URL of an API path, which must begin with `/`.
    pub fn api(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.url)
    }
}

/// The default location of the client's config, `~/.config/engram/cli.toml`.
pub fn default_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".config")
            .join("engram")
            .join("cli.toml")
    })
}

/// Environment first, then the file.
///
/// `env` is a closure so the precedence is testable without mutating the
/// process's own environment, which two tests running at once would race on.
///
/// Read with `toml_edit` rather than a second TOML crate: the config writer
/// already depends on it, and two keys do not justify a parser of their own.
pub fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    file: Option<&std::path::Path>,
) -> Result<Endpoint> {
    let mut from_file: (Option<String>, Option<String>) = (None, None);
    if let Some(p) = file.filter(|p| p.exists()) {
        let text = std::fs::read_to_string(p)
            .map_err(|e| Error::Validation(format!("{}: {e}", p.display())))?;
        let doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| Error::Validation(format!("{}: {e}", p.display())))?;
        let value = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_string);
        from_file = (value("url"), value("token"));
    }

    let url = env("ENGRAM_URL")
        .or(from_file.0)
        // The address a single-operator install is reached at, which is the
        // one this client is most often run beside.
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let token = env("ENGRAM_TOKEN").or(from_file.1).ok_or_else(|| {
        // The one error a first-time user will certainly see, so it says what
        // to do rather than what went wrong.
        Error::Validation(
            "no token: set ENGRAM_TOKEN, or write `token = \"engram_…\"` into \
             ~/.config/engram/cli.toml. Mint one under /ui/settings."
                .into(),
        )
    })?;

    Ok(Endpoint {
        url: url.trim_end_matches('/').to_string(),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn a_config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let (_dir, path) = a_config("url = \"https://file.test\"\ntoken = \"engram_file\"\n");
        let e = resolve(
            &env_of(&[
                ("ENGRAM_URL", "https://env.test"),
                ("ENGRAM_TOKEN", "engram_env"),
            ]),
            Some(&path),
        )
        .unwrap();
        assert_eq!(e.url, "https://env.test");
        assert_eq!(e.token, "engram_env");
    }

    #[test]
    fn the_file_is_read_when_the_environment_is_silent() {
        let (_dir, path) = a_config("url = \"https://file.test\"\ntoken = \"engram_file\"\n");
        let e = resolve(&env_of(&[]), Some(&path)).unwrap();
        assert_eq!(e.url, "https://file.test");
        assert_eq!(e.token, "engram_file");
    }

    #[test]
    fn a_missing_token_names_the_page_that_mints_one() {
        let err = resolve(&env_of(&[("ENGRAM_URL", "https://env.test")]), None).unwrap_err();
        let said = err.to_string();
        assert!(said.contains("/ui/settings"), "unhelpful: {said}");
        assert!(said.contains("ENGRAM_TOKEN"), "unhelpful: {said}");
    }

    #[test]
    fn a_trailing_slash_does_not_become_a_double_one() {
        let e = resolve(
            &env_of(&[
                ("ENGRAM_URL", "https://env.test/"),
                ("ENGRAM_TOKEN", "engram_env"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(e.api("/search"), "https://env.test/api/v1/search");
    }
}
