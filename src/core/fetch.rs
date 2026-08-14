use crate::config::CaptureConfig;
use crate::error::{Error, Result};

/// Retrieve a page for the paste-a-link door.
///
/// This is an anonymous client: no session, no subscription, no JavaScript
/// engine. It sees what a logged-out stranger sees, which is why it is the
/// *second* supplier into the extractor and not the only one — the extension
/// hands over a page the browser has already rendered and authenticated. What
/// this path can do is bounded by that, and its limits are its own.
///
/// Every failure is named rather than swallowed. The URL is operator input on
/// an authenticated endpoint, so an upstream 404 or a PDF where HTML was
/// expected is a bad request here, not a server fault — `Error::Validation`
/// carries the reason back and renders as 400.
///
/// Out of scope, deliberately: blocking loopback and private-range addresses.
/// The endpoint is authenticated and single-operator, so the only caller who
/// could aim it at the local network is the person who runs the machine.
pub async fn fetch_html(url: &url::Url, cfg: &CaptureConfig) -> Result<String> {
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
    let essence = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(essence.as_str(), "text/html" | "application/xhtml+xml") {
        return Err(Error::Validation(format!(
            "that URL is `{essence}`, not HTML"
        )));
    }

    // Streamed rather than `.text()`, because `Content-Length` is a claim and
    // the ceiling has to hold against a server that lies about it or omits it.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| Error::Validation(format!("fetch failed mid-transfer: {e}")))?
    {
        if bytes.len() + chunk.len() > cfg.fetch_max_bytes {
            return Err(Error::Validation(format!(
                "that page is larger than the {} byte fetch ceiling",
                cfg.fetch_max_bytes
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    String::from_utf8(bytes).map_err(|_| Error::Validation("that page is not valid UTF-8".into()))
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
