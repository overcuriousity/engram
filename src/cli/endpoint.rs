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
    let token = env("ENGRAM_TOKEN")
        .or(from_file.1)
        .ok_or_else(|| no_token(file))?;

    Ok(Endpoint {
        url: url.trim_end_matches('/').to_string(),
        token,
    })
}

/// The client's config as it is shipped and as it is scaffolded, which are one
/// text: the file in the release archive and the one written here cannot say
/// different things about the same two keys.
///
/// `token` is left commented. A run before the user has edited it must report a
/// missing token, not send `engram_…` to the server as though it were one.
const EXAMPLE: &str = include_str!("../../cli.example.toml");

/// The one error a first-time user will certainly see, so it says what to do
/// rather than what went wrong.
///
/// Where there is a path and nothing at it, the example is written there first
/// and the error says so: the missing half is a token, and the rest of the file
/// — which keys exist at all, what the URL defaults to — is not something the
/// user should have to be told in prose. A write that fails is not reported;
/// the reader wanted to search, and the way forward is the same either way.
fn no_token(file: Option<&std::path::Path>) -> Error {
    if let Some(p) = file.filter(|p| !p.exists())
        && scaffold(p).is_ok()
    {
        return Error::Validation(format!(
            "no token. Wrote an example config to {} — put a token in it, or \
             set ENGRAM_TOKEN. Mint one under /ui/settings.",
            p.display()
        ));
    }
    let where_to = match file {
        Some(p) => p.display().to_string(),
        None => "~/.config/engram/cli.toml".into(),
    };
    Error::Validation(format!(
        "no token: set ENGRAM_TOKEN, or write `token = \"engram_…\"` into \
         {where_to}. Mint one under /ui/settings."
    ))
}

/// Write the example config at `path`, creating the directory above it.
///
/// `0600` from the moment it exists, on the system where that means anything:
/// the file is written empty of credentials and the next thing that happens to
/// it is a user pasting a bearer token in, and a mode fixed after that is a
/// mode fixed too late.
fn scaffold(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, EXAMPLE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
    fn a_missing_token_with_no_config_writes_the_example_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram").join("cli.toml");
        let err = resolve(&env_of(&[]), Some(&path)).unwrap_err();

        let said = err.to_string();
        assert!(
            said.contains(&path.display().to_string()),
            "the error does not name the file it wrote: {said}"
        );
        assert!(said.contains("/ui/settings"), "unhelpful: {said}");
        assert!(path.exists(), "nothing was written to {}", path.display());
    }

    /// What is written has to be a config this same reader accepts — a scaffold
    /// that does not parse is worse than none — and it must not resolve, or the
    /// placeholder token would be sent to the server as if it were real.
    #[test]
    fn the_written_example_parses_and_still_has_no_token_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram").join("cli.toml");
        resolve(&env_of(&[]), Some(&path)).unwrap_err();

        let written = std::fs::read_to_string(&path).unwrap();
        written
            .parse::<toml_edit::DocumentMut>()
            .expect("the scaffold is not valid TOML");

        let e = resolve(&env_of(&[("ENGRAM_TOKEN", "engram_env")]), Some(&path)).unwrap();
        assert_eq!(e.url, "http://127.0.0.1:8080");
        resolve(&env_of(&[]), Some(&path))
            .expect_err("the placeholder token resolved as a real one");
    }

    /// The one thing this must never do. A config that is already there is the
    /// user's, token and all, and a tokenless run is not a reason to touch it.
    #[test]
    fn a_config_that_is_already_there_is_never_overwritten() {
        let (_dir, path) = a_config("url = \"https://file.test\"\n");
        let before = std::fs::read(&path).unwrap();

        let err = resolve(&env_of(&[]), Some(&path)).unwrap_err();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the config was rewritten"
        );
        assert!(err.to_string().contains("ENGRAM_TOKEN"), "unhelpful: {err}");
    }

    /// A scaffold that cannot be written is still an error about the token, not
    /// one about the filesystem: the user asked to search, and the way forward
    /// is the same either way.
    #[test]
    fn a_config_that_cannot_be_written_still_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the parent directory would have to be.
        let blocked = dir.path().join("wall");
        std::fs::write(&blocked, "").unwrap();

        let err = resolve(&env_of(&[]), Some(&blocked.join("cli.toml"))).unwrap_err();

        let said = err.to_string();
        assert!(said.contains("ENGRAM_TOKEN"), "unhelpful: {said}");
        assert!(said.contains("/ui/settings"), "unhelpful: {said}");
    }

    /// It is about to hold a bearer token, and the user pastes that in without
    /// being told to fix the mode first.
    #[cfg(unix)]
    #[test]
    fn the_written_config_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram").join("cli.toml");
        resolve(&env_of(&[]), Some(&path)).unwrap_err();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
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
