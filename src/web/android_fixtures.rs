//! The Android app's test fixtures are this server's real answers.
//!
//! `android/core/src/test/resources/api/*.json` is what the Kotlin models are
//! tested against. If those files were written by hand they would be a second
//! account of the API's shapes, and the first time a field was renamed here the
//! phone's tests would go on passing against a server that no longer exists.
//!
//! So the files are produced *by* this test, from the real routes over a small
//! base, and checked by it on every run: the committed file and a fresh answer
//! must have the same shape — the same keys, holding the same kinds of value.
//! Ids and clocks differ from run to run and are not compared.
//!
//! To refresh them after a deliberate change to a shape:
//!
//! ```text
//! ENGRAM_WRITE_ANDROID_FIXTURES=1 cargo test --lib android_fixtures
//! ```
//!
//! and then run the Android tests, which are the ones the change may break.

use crate::store::artifacts::{CorpusSpan, NewArtifact, NewMerged, SpanSource};
use crate::store::moments::{Kind, NewMoment, Source};
use crate::web::test_support::{app_with_cookie, json_of};
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use std::collections::BTreeSet;
use tower::ServiceExt;

/// Every key path in a document with the kind of value at it.
fn shape(v: &Value, at: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                shape(v, &format!("{at}.{k}"), out);
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.insert(format!("{at}[] (empty)"));
            }
            // Every element, into one set: rows of a list differ in which
            // optional keys they carry — a loose hit has `weak`, a sure one
            // does not — and the fixture is there to hold both kinds.
            for v in a {
                shape(v, &format!("{at}[]"), out);
            }
        }
        Value::Null => {
            out.insert(format!("{at}: null"));
        }
        Value::Bool(_) => {
            out.insert(format!("{at}: bool"));
        }
        Value::Number(_) => {
            out.insert(format!("{at}: number"));
        }
        Value::String(_) => {
            out.insert(format!("{at}: string"));
        }
    }
}

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("android/core/src/test/resources/api")
}

fn check(name: &str, fresh: &Value) {
    let path = dir().join(name);
    if std::env::var_os("ENGRAM_WRITE_ANDROID_FIXTURES").is_some() {
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(fresh).unwrap() + "\n").unwrap();
        return;
    }
    let held: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!("{name} is missing: run with ENGRAM_WRITE_ANDROID_FIXTURES=1")
        }))
        .unwrap();
    let (mut a, mut b) = (BTreeSet::new(), BTreeSet::new());
    shape(&held, "", &mut a);
    shape(fresh, "", &mut b);
    assert_eq!(
        a, b,
        "\nandroid/core/src/test/resources/api/{name} no longer has the shape this server \
         answers with.\nIf the change is meant, refresh the fixtures \
         (ENGRAM_WRITE_ANDROID_FIXTURES=1) and run the Android tests."
    );
}

#[tokio::test]
async fn the_android_fixtures_are_shapes_this_server_sends() {
    let mut core = crate::core::test_support::test_core().await;
    // Gaps and the retrieval figures are only recorded where searches are.
    core.learn.enabled = true;
    let s = core.store.clone();

    // A titled document with two artifacts cut from known lines, an untitled
    // one, and a journal entry.
    let doc = s
        .insert_corpus(
            "Filters narrow a search.\nPayload indexes make them fast.\nThird line.",
            "web",
            Some("Qdrant notes"),
        )
        .await
        .unwrap();
    s.insert_corpus("no title, only an opening", "paste", None)
        .await
        .unwrap();
    s.insert_corpus("Long day.", crate::core::ingest::ORIGIN_JOURNAL, None)
        .await
        .unwrap();
    let arts = s
        .insert_artifacts(
            &doc.id,
            &[0, 1].map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i} about payload filters"),
                corpus_span: Some(CorpusSpan {
                    start_line: 1,
                    end_line: 2,
                    source: SpanSource::Located,
                }),
                title: Some(format!("Payload filters {i}")),
                tags: vec!["qdrant".into()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let ids: Vec<String> = arts.iter().map(|a| a.id.clone()).collect();
    // Embedded, so the related route has a neighbour to list; and linked, so
    // it has something seen together to list beside it.
    crate::jobs::embed::run_corpus(&core, &doc.id)
        .await
        .unwrap();
    s.bump_link(
        &ids[0],
        &ids[1],
        5.0,
        Some("payload filter"),
        30.0,
        crate::store::now(),
    )
    .await
    .unwrap();
    // A merge of the two, and an earlier wording of it.
    let merged = s
        .insert_merged_artifact(
            &NewMerged {
                text: "Both, merged.".into(),
                title: Some("Payload filters".into()),
                ..Default::default()
            },
            &ids,
        )
        .await
        .unwrap();
    s.condense_artifact(
        &merged.id,
        None,
        "Merged, shorter.",
        None,
        &["loses a detail".into()],
        serde_json::json!({}),
    )
    .await
    .unwrap();
    // A reminder and a date, both today, and a sitting that opened the merge.
    let now = crate::store::now();
    for (kind, source, span) in [
        (Kind::Due, Source::Set, None),
        (Kind::Event, Source::Extracted, Some("today".to_string())),
    ] {
        s.insert_moment(&NewMoment {
            artifact_id: merged.id.clone(),
            kind,
            at: Some(now),
            tz: "UTC".into(),
            rule: None,
            source,
            span,
            series_id: None,
        })
        .await
        .unwrap();
    }
    s.insert_pursuit(
        now,
        &["payload filter".into()],
        std::slice::from_ref(&merged.id),
        None,
    )
    .await
    .unwrap();

    // The three shapes the judging screens read, each in its own corpus so
    // nothing here is entangled with the merge above: a pair waiting on an
    // answer, an artifact the base hid (the undo list's plainest row), and a
    // question nothing covered.
    let pair_doc = s
        .insert_corpus("timeouts", "web", Some("Timeouts"))
        .await
        .unwrap();
    let pair_sides = s
        .insert_artifacts(
            &pair_doc.id,
            &[0, 1].map(|i| NewArtifact {
                ordinal: i,
                text: format!("the timeout is {} seconds", 30 + i * 60),
                title: Some(format!("Timeout {i}")),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    s.record_pair(&pair_sides[0].id, &pair_sides[1].id, 0.91)
        .await
        .unwrap();

    let old_doc = s
        .insert_corpus("an older way", "web", Some("Older"))
        .await
        .unwrap();
    let hidden = s
        .insert_artifacts(
            &old_doc.id,
            &[NewArtifact {
                ordinal: 0,
                text: "an older way of saying it".into(),
                title: Some("Superseded".into()),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    s.set_superseded_by(&hidden[0].id, Some(&merged.id))
        .await
        .unwrap();

    // The embed model is the one the gap reader filters on, so it is asked
    // for rather than spelled.
    let ask = s
        .record_ask(crate::store::asks::NewAsk {
            question: "how do ticks work".into(),
            filters: "{}".into(),
            query_vec: vec![1.0, 0.0],
            embed_model: core.embedder.model().into(),
            answer: "Not in the knowledge base.".into(),
            abstained: true,
            ..Default::default()
        })
        .await
        .unwrap();
    s.judge_ask(&ask, crate::store::asks::AskVerdict::NothingHere)
        .await
        .unwrap();

    let today = chrono::DateTime::from_timestamp(doc.created_at, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let (app, cookie) = app_with_cookie(core).await;
    let get = |uri: String| {
        let (app, cookie) = (app.clone(), cookie.clone());
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header("cookie", cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), 200, "{uri}");
            json_of(res).await
        }
    };

    check("corpora.json", &get("/api/v1/corpora".into()).await);
    check(
        "corpus.json",
        &get(format!("/api/v1/corpora/{}", doc.id)).await,
    );
    check(
        "artifact.json",
        &get(format!("/api/v1/artifacts/{}", ids[0])).await,
    );
    check(
        "lineage.json",
        &get(format!("/api/v1/artifacts/{}/lineage", merged.id)).await,
    );
    check(
        "related.json",
        &get(format!("/api/v1/artifacts/{}/related", ids[0])).await,
    );
    check(
        "source.json",
        &get(format!("/api/v1/artifacts/{}/source", ids[0])).await,
    );
    check("status.json", &get("/api/v1/status".into()).await);
    check(
        "about.json",
        &get(format!("/api/v1/artifacts/{}/about", ids[0])).await,
    );
    check(
        "bands.json",
        &get(format!("/api/v1/corpora/{}/bands", doc.id)).await,
    );
    check("facets.json", &get("/api/v1/facets".into()).await);
    check("feedback.json", &get("/api/v1/feedback".into()).await);
    check("lang.json", &get("/api/v1/settings/lang".into()).await);
    check("notify.json", &get("/api/v1/settings/notify".into()).await);
    check(
        "machine.json",
        &get("/api/v1/insights/machine".into()).await,
    );
    check("report.json", &get("/api/v1/insights/report".into()).await);
    check(
        "versions.json",
        &get(format!("/api/v1/artifacts/{}/versions", merged.id)).await,
    );
    check(
        "day.json",
        &get(format!("/api/v1/days/{today}?tz=UTC")).await,
    );
    check(
        "moments.json",
        &get("/api/v1/moments?kind=due".into()).await,
    );
    check("pairs.json", &get("/api/v1/pairs".into()).await);
    check("gaps.json", &get("/api/v1/gaps".into()).await);
    check("insights.json", &get("/api/v1/insights".into()).await);
    check(
        "set_aside.json",
        &get("/api/v1/insights/set-aside".into()).await,
    );

    // Three shapes no small base produces on demand — a loose hit past the
    // cliff, an offer, a finished answer — serialised by the code the routes
    // use, from values built here.
    let hits = vec![
        crate::cli::search::fixture::hit("sure", 0.9, false, false),
        crate::cli::search::fixture::hit("loose", 0.2, true, true),
    ];
    // As the app's door answers it: the event the list was recorded under
    // beside the rows, which is what an open, a verdict and a gap name.
    let mut search = crate::web::api::search_body(&hits, None).unwrap();
    search["event"] = serde_json::Value::String("ev-search".into());
    check("search.json", &search);
    check(
        "offer.json",
        &serde_json::json!({ "offer": crate::web::api::OfferCard {
            artifact_id: merged.id.clone(),
            label: "Payload filters".into(),
            named: true,
            snippet: "Both, merged.".into(),
            rung: "pattern",
            slot: Some(3),
            events: 4,
            blocks: vec!["weekday", "hour"],
            at: Some(now),
            at_tz: Some("Europe/Berlin".into()),
        }}),
    );
    check(
        "ask_done.json",
        &serde_json::to_value(crate::core::ask::AskResponse {
            answer: "Run `qdrant-cli index create` on the payload field.".into(),
            citations: hits,
            dropped: 2,
            truncated: false,
            abstained: false,
            unsupported: vec!["qdrant-cli index create".into()],
            retired_only: false,
            event_id: None,
        })
        .unwrap(),
    );
}
