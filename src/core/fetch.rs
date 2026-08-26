use crate::config::CaptureConfig;
use crate::error::{Error, Result};

/// What a URL turned out to hold, of the kinds some door here can read.
#[derive(Debug)]
pub enum Fetched {
    /// A page, decoded to text. What the extractor reads.
    Html(String),
    /// A PDF, as sent. What `Stage::Extract` reads.
    Pdf(Vec<u8>),
    /// An image, as sent, with the type the server named it.
    Image { mime: String, bytes: Vec<u8> },
}

/// Retrieve a page for the paste-a-link door.
///
/// The page-only face of `fetch`: a PDF or an image where HTML was expected
/// is a bad request here, named by type.
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

/// Retrieve a document — a page, a PDF or an image — for the two doors that
/// take a link: paste-a-link and MCP.
///
/// This is an anonymous client: no session, no subscription, no JavaScript
/// engine. It sees what a logged-out stranger sees, which is why it is the
/// *second* supplier into the extractor and not the only one — the extension
/// hands over a page the browser has already rendered and authenticated. What
/// this path can do is bounded by that, and its limits are its own.
///
/// Every failure is named rather than swallowed. The URL is operator input on
/// an authenticated endpoint, so an upstream 404 or a video where a document
/// was expected is a bad request here, not a server fault — `Error::Validation`
/// carries the reason back and renders as 400.
///
/// Each kind is held to the ceiling its upload door already applies: a page
/// to `fetch_max_bytes`, a PDF to `pdf_max_bytes`, an image to
/// `image_max_bytes`. A book is tens of megabytes and a page is not.
///
/// Out of scope, deliberately: blocking loopback and private-range addresses.
/// The endpoint is authenticated and single-operator, so the only caller who
/// could aim it at the local network is the person who runs the machine.
pub async fn fetch(url: &url::Url, cfg: &CaptureConfig) -> Result<Fetched> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Validation(format!(
            "unsupported scheme `{}` — only http and https are fetched",
            url.scheme()
        )));
    }

    let client = reqwest::Client::builder()
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

    // Checked before reading the body, so a 200 MB video is refused by name
    // rather than fed to the extractor a chunk at a time.
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

    // Streamed rather than `.bytes()`, because `Content-Length` is a claim and
    // the ceiling has to hold against a server that lies about it or omits it.
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

/// The body as text, in whatever encoding it was actually sent in.
///
/// Refusing everything that is not UTF-8 would refuse a real share of the
/// pages worth pasting a link to: Windows-1252 and Latin-1 are still what an
/// older site serves, and an older site is exactly the kind whose page is
/// worth keeping. The charset is a parameter on `Content-Type`, and the header
/// has already been read; `reqwest`'s own `.text()` honours it, and this path
/// streams instead so as to hold the byte ceiling against a lying
/// `Content-Length`.
///
/// Nothing here can fail. An encoding nobody names falls back to UTF-8, and
/// `encoding_rs` substitutes the replacement character for bytes that do not
/// decode rather than giving up on the document — a page with one bad byte in
/// a footer is still the page.
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
    async fn a_page_that_is_not_utf8_is_decoded_rather_than_refused() {
        // Windows-1252 is still what a good deal of the older web serves, and
        // an older page is exactly the kind worth keeping a copy of. Reading
        // the bytes as UTF-8 and giving up refused those pages outright.
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
        // Quoted, spaced, and cased however the server felt like writing it.
        assert_eq!(
            decode(b"caf\xe9", "text/html; Charset=\"ISO-8859-1\""),
            "café"
        );
        // An encoding nobody has heard of is read as UTF-8 rather than
        // refused: a wrong guess renders badly, and refusing loses the page.
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

    /// The paste-a-link door and the MCP door both point at documents, and a
    /// document at a URL is as often a PDF as a page.
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

    /// A book is tens of megabytes and a page is not; each kind is held to
    /// its own ceiling, the one the upload doors already apply.
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
