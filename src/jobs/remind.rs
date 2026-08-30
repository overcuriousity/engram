//! Push for what is due: the channels a user configured, and the unit that
//! sleeps until the next due moment and posts it.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Gotify { url: String, token: String },
    UnifiedPush { endpoint: String },
}

/// The channels in a user's `notify` JSON. A Gotify entry needs both its url
/// and its token; a UnifiedPush entry is its endpoint.
pub fn notify_targets(notify: &serde_json::Value) -> Vec<Target> {
    let mut out = vec![];
    if let (Some(url), Some(token)) = (notify["gotify"]["url"].as_str(), notify["gotify"]["token"].as_str())
        && !url.is_empty()
        && !token.is_empty()
    {
        out.push(Target::Gotify { url: url.into(), token: token.into() });
    }
    if let Some(e) = notify["unifiedpush"]["endpoint"].as_str()
        && !e.is_empty()
    {
        out.push(Target::UnifiedPush { endpoint: e.into() });
    }
    out
}

/// One POST per channel, no library. Gotify takes a JSON body and the token
/// in a header; UnifiedPush takes the message as the body.
pub async fn push(http: &reqwest::Client, target: &Target, title: &str, message: &str) -> crate::error::Result<()> {
    let res = match target {
        Target::Gotify { url, token } => {
            http.post(url)
                .header("X-Gotify-Key", token)
                .json(&serde_json::json!({ "title": title, "message": message, "priority": 5 }))
                .send()
                .await
        }
        Target::UnifiedPush { endpoint } => http.post(endpoint).body(format!("{title}\n{message}")).send().await,
    };
    let res = res.map_err(|e| crate::error::Error::Inference { role: "push", detail: e.to_string() })?;
    if !res.status().is_success() {
        return Err(crate::error::Error::Inference { role: "push", detail: format!("HTTP {}", res.status()) });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_read_from_the_namespaced_json() {
        assert!(notify_targets(&serde_json::json!({})).is_empty());
        assert_eq!(
            notify_targets(&serde_json::json!({"gotify": {"url": "u", "token": ""}})),
            vec![],
            "a token is required"
        );
        assert_eq!(
            notify_targets(&serde_json::json!({"unifiedpush": {"endpoint": "e"}})),
            vec![Target::UnifiedPush { endpoint: "e".into() }]
        );
    }
}
