use crate::config::CaptureConfig;
use crate::error::{Error, Result};

#[derive(Debug)]
pub enum Fetched {
    Html(String),
    Pdf(Vec<u8>),
    Image { mime: String, bytes: Vec<u8> },
}

/// The scheme rule for every door that takes a URL; `Url::parse` accepts `javascript:` and `data:`.
pub fn ensure_readable(url: &url::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Validation(format!(
            "unsupported scheme `{}` — only http and https are read",
            url.scheme()
        )));
    }
    Ok(())
}

pub fn parse_readable(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
    ensure_readable(&url)?;
    Ok(url)
}

/// Share sheets hand links over as `text/plain`; one http(s) token only, so prose stays prose.
pub fn only_a_url(body: &str) -> Option<url::Url> {
    let trimmed = body.trim();
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    let u = url::Url::parse(trimmed).ok()?;
    // A lone `javascript:` word is text somebody typed, not a link.
    ensure_readable(&u).is_ok().then_some(u)
}

pub async fn fetch_html(url: &url::Url, cfg: &CaptureConfig) -> Result<String> {
    match fetch(url, cfg).await? {
        Fetched::Html(text) => Ok(text),
        Fetched::Pdf(_) => Err(Error::Validation(
            "that URL is `application/pdf`, not HTML".into(),
        )),
        Fetched::Image { mime, .. } => {
            Err(Error::Validation(format!("that URL is `{mime}`, not HTML")))
        }
    }
}

/// Some servers refuse anonymous clients.
pub const USER_AGENT: &str = concat!(
    "engram/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/overcuriousity/engram)"
);

/// Retrieve a page, PDF or image for paste-a-link and MCP, as an anonymous client.
/// Failures are `Error::Validation` (400): the URL is operator input on an authenticated endpoint.
/// Private addresses are not blocked: the single authenticated operator runs the machine.
pub async fn fetch(url: &url::Url, cfg: &CaptureConfig) -> Result<Fetched> {
    ensure_readable(url)?;

    // Wikipedia answers the default `reqwest/x.y` agent with a 403.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/pdf;q=0.9,*/*;q=0.8",
        ),
    );
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(cfg.fetch_timeout_secs))
        // A redirect chain that never ends is a timeout dressed up as
        // progress. Ten is what every other client settles on.
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| Error::Validation(format!("could not build a client: {e}")))?;

    let mut res = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| Error::Validation(format!("fetch failed: {e}")))?;

    if !res.status().is_success() {
        return Err(Error::Validation(format!(
            "fetch failed: the server answered {}",
            res.status()
        )));
    }

    // Checked before reading the body, so a 200 MB video is refused by name.
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    enum Kind {
        Html,
        Pdf,
        Image,
    }
    let (kind, ceiling, what) = match essence.as_str() {
        "text/html" | "application/xhtml+xml" => (Kind::Html, cfg.fetch_max_bytes, "page"),
        "application/pdf" => (Kind::Pdf, cfg.pdf_max_bytes, "PDF"),
        e if e.starts_with("image/") => (Kind::Image, cfg.image_max_bytes, "image"),
        _ => {
            return Err(Error::Validation(format!(
                "that URL is `{essence}` — only a page, a PDF or an image is read"
            )));
        }
    };

    // Streamed: `Content-Length` is a claim, and the ceiling must hold when a server lies about it.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| Error::Validation(format!("fetch failed mid-transfer: {e}")))?
    {
        if bytes.len() + chunk.len() > ceiling {
            return Err(Error::Validation(format!(
                "that {what} is larger than the {ceiling} byte fetch ceiling"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(match kind {
        Kind::Html => Fetched::Html(decode(&bytes, &content_type)),
        Kind::Pdf => Fetched::Pdf(bytes),
        Kind::Image => Fetched::Image {
            mime: essence,
            bytes,
        },
    })
}

/// The body as text in its declared charset, falling back to UTF-8; never fails.
fn decode(bytes: &[u8], content_type: &str) -> String {
    let label = content_type
        .split(';')
        .skip(1)
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("charset"))
        .map(|(_, v)| v.trim().trim_matches('"'))
        .unwrap_or("utf-8");

    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> CaptureConfig {
        CaptureConfig::default()
    }

    #[tokio::test]
    async fn fetch_refuses_a_non_http_scheme() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.test/x",
            "data:text/html,x",
        ] {
            let u = url::Url::parse(bad).unwrap();
            let err = fetch_html(&u, &cfg()).await.unwrap_err();
            assert!(
                matches!(err, Error::Validation(ref m) if m.contains("scheme")),
                "accepted {bad}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_returns_the_body_of_an_html_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body><p>hello</p></body></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/page", server.uri())).unwrap();
        let body = fetch_html(&u, &cfg()).await.unwrap();
        assert!(body.contains("hello"));
    }

    #[tokio::test]
    async fn the_request_says_who_is_asking() {
        // `header_regex`: wiremock reads a comma in an exact value as a list of values.
        use wiremock::matchers::{header, header_regex};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/named"))
            .and(header("user-agent", USER_AGENT))
            .and(header_regex(
                "accept",
                r"^text/html,application/xhtml\+xml,application/pdf;q=0\.9,\*/\*;q=0\.8$",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body><p>named</p></body></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/named", server.uri())).unwrap();
        let body = fetch_html(&u, &cfg()).await.unwrap();
        assert!(body.contains("named"));
        assert!(USER_AGENT.starts_with("engram/"), "{USER_AGENT}");
    }

    #[tokio::test]
    async fn a_page_that_is_not_utf8_is_decoded_rather_than_refused() {
        let server = MockServer::start().await;
        // "Grüße" in Windows-1252: ü is 0xFC, ß is 0xDF, neither valid UTF-8.
        let body = b"<html><body><p>Gr\xFC\xDFe</p></body></html>".to_vec();
        Mock::given(method("GET"))
            .and(path("/latin"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body, "text/html; charset=windows-1252"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/latin", server.uri())).unwrap();
        let out = fetch_html(&u, &cfg()).await.unwrap();
        assert!(out.contains("Grüße"), "got {out}");
    }

    #[test]
    fn the_charset_is_read_off_the_header_and_defaults_to_utf8() {
        assert_eq!(decode(b"caf\xe9", "text/html; charset=iso-8859-1"), "café");
        assert_eq!(decode("café".as_bytes(), "text/html"), "café");
        assert_eq!(
            decode(b"caf\xe9", "text/html; Charset=\"ISO-8859-1\""),
            "café"
        );
        assert_eq!(
            decode("café".as_bytes(), "text/html; charset=made-up"),
            "café"
        );
    }

    #[tokio::test]
    async fn fetch_refuses_a_non_html_content_type_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"%PDF-1.7".to_vec(), "application/pdf"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/doc.pdf", server.uri())).unwrap();
        let err = fetch_html(&u, &cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("application/pdf")),
            "the refused type must be named: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_hands_back_a_pdf_as_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"%PDF-1.7".to_vec(), "application/pdf"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/doc.pdf", server.uri())).unwrap();
        match fetch(&u, &cfg()).await.unwrap() {
            Fetched::Pdf(bytes) => assert_eq!(bytes, b"%PDF-1.7"),
            other => panic!("not a pdf: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_hands_back_an_image_with_its_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 2, 3], "image/png"))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/pic", server.uri())).unwrap();
        match fetch(&u, &cfg()).await.unwrap() {
            Fetched::Image { mime, bytes } => {
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, vec![1, 2, 3]);
            }
            other => panic!("not an image: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fetched_pdf_is_held_to_the_pdf_ceiling_not_the_page_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![b'x'; 4096], "application/pdf"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/doc.pdf", server.uri())).unwrap();
        let page_small = CaptureConfig {
            fetch_max_bytes: 1024,
            ..CaptureConfig::default()
        };
        assert!(fetch(&u, &page_small).await.is_ok());
        let pdf_small = CaptureConfig {
            pdf_max_bytes: 1024,
            ..CaptureConfig::default()
        };
        let err = fetch(&u, &pdf_small).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("1024")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_refuses_a_type_nothing_here_reads_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clip"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0; 8], "video/mp4"))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/clip", server.uri())).unwrap();
        let err = fetch(&u, &cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("video/mp4")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_stops_at_the_byte_ceiling() {
        let server = MockServer::start().await;
        let big = "x".repeat(4096);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(big, "text/html"))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/big", server.uri())).unwrap();
        let small = CaptureConfig {
            fetch_max_bytes: 1024,
            ..CaptureConfig::default()
        };
        let err = fetch_html(&u, &small).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("1024")),
            "the ceiling must be named: {err:?}"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_status_is_named_not_swallowed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/gone", server.uri())).unwrap();
        let err = fetch_html(&u, &cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("404")),
            "the status must reach the caller: {err:?}"
        );
    }
}
