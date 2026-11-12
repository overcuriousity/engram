//! Which engram this client is talking to, and with what credential.

use crate::error::{Error, Result};

/// Version-stamped, so a server log says which build asked.
pub(crate) const USER_AGENT: &str = concat!("engram-cli/", env!("CARGO_PKG_VERSION"));

/// The one HTTP client every verb uses.
pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))
}

pub struct Endpoint {
    /// No trailing slash, so joining is concatenation.
    pub url: String,
    pub token: String,
}

/// Not derived: a derived one prints the bearer token wherever a `Result` is rendered.
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

    /// A request that got no answer, said as the address and the `io::Error` cause from the chain;
    /// reqwest's own message hides the cause.
    pub(crate) fn unreachable(&self, err: reqwest::Error) -> Error {
        if !(err.is_connect() || err.is_timeout()) {
            return Error::Validation(err.to_string());
        }
        let mut io = None;
        let mut innermost: &dyn std::error::Error = &err;
        while let Some(next) = innermost.source() {
            innermost = next;
            if let Some(e) = next.downcast_ref::<std::io::Error>() {
                io = Some(e);
            }
        }
        let cause = match io {
            _ if err.is_timeout() => "timed out".to_string(),
            Some(e) if e.raw_os_error().is_some() => e.kind().to_string(),
            Some(e) => e.to_string(),
            None => innermost.to_string(),
        };
        Error::Validation(format!("could not reach {}: {cause}", self.url))
    }
}

pub fn default_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".config")
            .join("engram")
            .join("cli.toml")
    })
}

/// Environment first, then the file. `env` is a closure so tests need not mutate the process
/// environment; `toml_edit` keeps a hand-edited file's comments.
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
        // Where a single-operator install is reached.
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let token = env("ENGRAM_TOKEN")
        .or(from_file.1)
        .ok_or_else(|| no_token(file))?;

    Ok(Endpoint {
        url: url.trim_end_matches('/').to_string(),
        token,
    })
}

/// The client's config as shipped and as scaffolded, one text. `token` stays commented so an
/// unedited file reports a missing token rather than sending the placeholder.
const EXAMPLE: &str = include_str!("../../cli.example.toml");

/// The error every first-time user sees, so it says what to do; scaffolds the example where
/// nothing is at `file`, a failed write unreported.
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

/// Write the example config at `path`, `0600` from creation: a bearer token is pasted in next.
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

    #[tokio::test]
    async fn nothing_listening_is_said_as_the_address_and_the_refusal() {
        // Bound and dropped, so nothing answers on the port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = Endpoint {
            url: format!("http://127.0.0.1:{port}"),
            token: "engram_x".into(),
        };
        let err = client()
            .unwrap()
            .get(e.api("/status"))
            .send()
            .await
            .unwrap_err();
        let Error::Validation(msg) = e.unreachable(err) else {
            panic!("a validation error")
        };
        assert_eq!(
            msg,
            format!("could not reach http://127.0.0.1:{port}: connection refused")
        );
    }

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

    #[test]
    fn a_config_that_cannot_be_written_still_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("wall");
        std::fs::write(&blocked, "").unwrap();

        let err = resolve(&env_of(&[]), Some(&blocked.join("cli.toml"))).unwrap_err();

        let said = err.to_string();
        assert!(said.contains("ENGRAM_TOKEN"), "unhelpful: {said}");
        assert!(said.contains("/ui/settings"), "unhelpful: {said}");
    }

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
