use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::corpora::{
    Corpus, CorpusStatus, Followup, Insertion, NearDuplicate, content_hash,
};
use crate::store::jobs::Stage;
use crate::store::now;

/// Its own value, so the queue and the detail page can tell a photo from a paste.
pub const ORIGIN_IMAGE: &str = "image";
/// Its own value, so an extracted document is told apart from typed text.
pub const ORIGIN_PDF: &str = "pdf";
/// Text typed or pasted into the capture box.
pub const ORIGIN_WEB: &str = "web";
/// A page read from a pasted link, by this server, as a stranger.
pub const ORIGIN_FETCH: &str = "fetch";
/// The Android share target, the iOS Shortcut and the bookmarklet alike.
pub const ORIGIN_SHARE: &str = "share";
/// Its own value: a corpus a model wrote must never read as one a person typed.
pub const ORIGIN_ASK: &str = "ask";
/// Bounds one artifact however it arrived; the same figure as `web::MAX_BODY_BYTES`, which bounds a
/// body.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Why bytes offered as text were not taken as text.
#[derive(Debug, PartialEq, Eq)]
pub enum NotText {
    /// Not UTF-8, and no byte-order mark saying it is anything else.
    Undecodable,
    /// Decodes, but holds a NUL — which no text does on purpose.
    Nul,
}

impl NotText {
    /// The doors word `Undecodable` themselves; this one has a single cause and remedy.
    pub const NUL_MESSAGE: &str = "that text holds NUL bytes — it is probably UTF-16 saved \
         without a byte-order mark. Save it as UTF-8 and send it again.";
}

/// UTF-16 with a BOM is decoded; anything else must be UTF-8 and is refused, never converted
/// lossily. A NUL is refused: BOM-less UTF-16LE of ASCII text is valid UTF-8 full of NULs.
pub fn decode_text(bytes: Vec<u8>) -> std::result::Result<String, NotText> {
    let text = match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => utf16(encoding_rs::UTF_16LE, rest)?,
        [0xFE, 0xFF, rest @ ..] => utf16(encoding_rs::UTF_16BE, rest)?,
        _ => String::from_utf8(bytes).map_err(|_| NotText::Undecodable)?,
    };
    if text.contains('\0') {
        return Err(NotText::Nul);
    }
    Ok(text)
}

fn utf16(
    encoding: &'static encoding_rs::Encoding,
    bytes: &[u8],
) -> std::result::Result<String, NotText> {
    encoding
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|t| t.into_owned())
        .ok_or(NotText::Undecodable)
}

/// Bounds only the copy in the describe prompt; the note itself is stored whole.
pub const MAX_NOTE_CHARS: usize = 2000;

/// Trimmed; `None` when there is nothing in it.
fn clean_note(note: Option<String>) -> Option<String> {
    let n = note?.trim().to_string();
    if n.is_empty() {
        return None;
    }
    Some(n)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestOutcome {
    pub id: String,
    pub status: CorpusStatus,
    /// The text was already stored byte for byte and no new source was created.
    pub duplicate: bool,
    /// Close to something stored: stored and searchable anyway, flagged for a person to decide
    /// later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_duplicate: Option<NearDuplicate>,
    /// Why a link could not be read; it is stored anyway and retried. See `Core::ingest_link`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_unread: Option<String>,
}

impl IngestOutcome {
    /// The answer for text or bytes already stored: the corpus that has them.
    fn existing(c: &Corpus) -> Self {
        IngestOutcome {
            id: c.id.clone(),
            status: c.status,
            duplicate: true,
            near_duplicate: None,
            link_unread: None,
        }
    }
}

/// One PDF, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct PdfCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
    /// See `infer::lang`; `Lang::default()` is English, which every door that knows no language
    /// sends.
    pub lang: crate::infer::lang::Lang,
}

/// One image, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct ImageCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
    /// See `infer::lang`; `Lang::default()` is English, which every door that knows no language
    /// sends.
    pub lang: crate::infer::lang::Lang,
}

/// One capture, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct Capture {
    pub text: String,
    /// The channel: `web`, `mcp`, `extension`, `fetch`, `upload`.
    pub origin: String,
    pub title_hint: Option<String>,
    /// Provenance, never an instruction: nothing downstream fetches this.
    pub source_url: Option<String>,
    /// What the door knew beyond the text. Namespaced; see the schema comment.
    pub metadata: serde_json::Value,
}

impl Capture {
    pub fn new(text: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            origin: origin.into(),
            title_hint: None,
            source_url: None,
            metadata: serde_json::json!({}),
        }
    }

    pub fn with_note(mut self, note: Option<String>) -> Self {
        if let Some(n) = clean_note(note) {
            self.metadata["note"] = serde_json::Value::String(n);
        }
        self
    }

    /// The `file` facts of an uploaded text file.
    pub fn with_file(mut self, name: Option<&str>, size: usize, mime: &str) -> Self {
        let mut f = serde_json::json!({ "size": size, "mime": mime });
        if let Some(n) = name {
            f["name"] = serde_json::Value::String(n.to_string());
        }
        self.metadata["file"] = f;
        self
    }

    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title_hint = title;
        self
    }

    /// So a date read out of the text is resolved where it was written.
    pub fn with_tz(mut self, tz: Option<String>) -> Self {
        if let Some(t) = tz.filter(|t| !t.trim().is_empty()) {
            self.metadata["tz"] = serde_json::Value::String(t);
        }
        self
    }

    /// Stamped at the door: a job's `Core` is cached per tenant and knows no subject.
    pub fn with_lang(mut self, lang: crate::infer::lang::Lang) -> Self {
        self.metadata["lang"] = serde_json::Value::String(lang.tag().to_string());
        self
    }

    /// A hint to the judged read, not an override; a forced reminder too large to judge is refused
    /// at the door.
    pub fn with_intent(mut self, intent: Option<crate::core::moments::Intent>) -> Self {
        if let Some(i) = intent {
            self.metadata["intent"] = serde_json::Value::String(i.as_str().into());
        }
        self
    }

    pub fn with_source_url(mut self, url: Option<String>) -> Self {
        self.source_url = url;
        self
    }

    /// Provenance, never an instruction: a corpus a model wrote says so and what it was written
    /// from.
    pub fn with_ask(
        mut self,
        ask_id: &str,
        question: &str,
        citations: &[crate::store::asks::AskCitation],
    ) -> Self {
        self.metadata["ask"] = serde_json::json!({
            "event_id": ask_id,
            "question": question,
            "artifact_ids": citations
                .iter()
                .map(|c| c.artifact_id.as_str())
                .collect::<Vec<_>>(),
        });
        self
    }

    /// Links the capture to the search it was typed into, so that empty search does not become a
    /// gap. The id comes off a page: the door checks `Store::event_is_mine`.
    pub fn with_search(mut self, event_id: &str) -> Self {
        self.metadata["search"] = serde_json::json!({ "event_id": event_id });
        self
    }
}

/// The search a capture was typed from, where its door recorded one.
pub fn typed_from(metadata: &serde_json::Value) -> Option<&str> {
    metadata
        .get("search")?
        .get("event_id")?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// What a link held: a page extracted, or a PDF or image as sent.
#[derive(Debug)]
pub(crate) enum ReadLink {
    Page {
        markdown: String,
        title: Option<String>,
    },
    Pdf(Vec<u8>),
    Image(Vec<u8>),
}

/// `plan.pdf` out of `/papers/plan.pdf`; nothing out of `/` or `/papers/`.
fn url_filename(url: &url::Url) -> Option<String> {
    url.path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// What a door that asked for a reminder is told on the embed-only floor.
pub const NO_SYNTHESIZER_REMINDER: &str = "reminders are not configured: a reminder is read by the model [infer.synthesize] \
     names, and this base has none. Capture the text with `engram -c`, or configure \
     [infer.synthesize].";

impl Core {
    /// No inference call: capture must stay instant and survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        self.ingest_capture(Capture::new(text, origin).with_title(title_hint.map(str::to_string)))
            .await
    }

    /// Every link door comes through here; a link that cannot be read is stored anyway.
    pub async fn ingest_url(
        &self,
        url: &url::Url,
        title: Option<String>,
        note: Option<String>,
        lang: crate::infer::lang::Lang,
    ) -> Result<IngestOutcome> {
        self.ingest_link(url, title, note, None, lang).await
    }

    /// An unreadable link is stored with what came with it and retried by a `Fetch` unit.
    /// `shared` is kept only then: a share sheet's text is usually the page's title again.
    pub async fn ingest_link(
        &self,
        url: &url::Url,
        title: Option<String>,
        note: Option<String>,
        shared: Option<String>,
        lang: crate::infer::lang::Lang,
    ) -> Result<IngestOutcome> {
        // A scheme nothing reads is not a link that failed; it is not a link.
        crate::core::fetch::ensure_readable(url)?;
        match self.read_link(url).await {
            Ok(read) => self.ingest_read(url, read, title, note, lang).await,
            Err(Error::Validation(why)) => {
                self.hold_link(url, title, note, shared, lang, &why).await
            }
            Err(e) => Err(e),
        }
    }

    /// Everything that can refuse a link is here, so storing and retrying agree on what refused
    /// means.
    pub(crate) async fn read_link(&self, url: &url::Url) -> Result<ReadLink> {
        use crate::core::fetch::Fetched;
        Ok(match crate::core::fetch::fetch(url, &self.capture).await? {
            Fetched::Html(html) => {
                let page = crate::core::extract::extract(
                    html,
                    Some(url.clone()),
                    self.capture.min_extracted_chars,
                )
                .await?;
                ReadLink::Page {
                    markdown: page.markdown,
                    title: page.title,
                }
            }
            Fetched::Pdf(bytes) => ReadLink::Pdf(bytes),
            Fetched::Image { bytes, .. } => ReadLink::Image(bytes),
        })
    }

    async fn ingest_read(
        &self,
        url: &url::Url,
        read: ReadLink,
        title: Option<String>,
        note: Option<String>,
        lang: crate::infer::lang::Lang,
    ) -> Result<IngestOutcome> {
        let source_url = Some(url.to_string());
        match read {
            ReadLink::Page {
                markdown,
                title: page_title,
            } => {
                // Readability drops the `<h1>`, so the page's title beats `derive_title`'s first
                // heading.
                self.ingest_capture(
                    Capture::new(markdown, ORIGIN_FETCH)
                        .with_lang(lang)
                        .with_title(title.or(page_title))
                        .with_note(note)
                        .with_source_url(source_url),
                )
                .await
            }
            ReadLink::Pdf(bytes) => {
                self.ingest_pdf_from(
                    PdfCapture {
                        bytes,
                        filename: url_filename(url),
                        title_hint: title,
                        note,
                        lang,
                    },
                    source_url,
                )
                .await
            }
            ReadLink::Image(bytes) => {
                self.ingest_image_from(
                    ImageCapture {
                        bytes,
                        filename: url_filename(url),
                        title_hint: title,
                        note,
                        lang,
                    },
                    source_url,
                )
                .await
            }
        }
    }

    /// Stored and embedded like any capture, with a `Fetch` unit armed to retry the page.
    async fn hold_link(
        &self,
        url: &url::Url,
        title: Option<String>,
        note: Option<String>,
        shared: Option<String>,
        lang: crate::infer::lang::Lang,
        why: &str,
    ) -> Result<IngestOutcome> {
        let shared = shared
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != url.as_str());
        let text = match &shared {
            Some(s) => format!("{url}\n\n{s}"),
            None => url.to_string(),
        };
        let mut c = Capture::new(text, ORIGIN_FETCH)
            .with_lang(lang)
            .with_title(title.clone())
            .with_note(note)
            .with_source_url(Some(url.to_string()));
        c.metadata["fetch"] = serde_json::json!({
            "pending": true,
            "error": why,
            // A page arriving later may name itself only where nobody else did.
            "title_given": title.is_some(),
        });
        let mut out = self.ingest_capture(c).await?;
        if !out.duplicate {
            self.store.enqueue(Stage::Fetch, "corpus", &out.id).await?;
        }
        out.link_unread = Some(why.to_string());
        tracing::info!(
            corpus_id = %out.id,
            url = %url,
            reason = why,
            "link could not be read; stored as the link and trying again later"
        );
        Ok(out)
    }

    /// A page replaces the text in place, keeping the id; a PDF or image becomes its own capture.
    pub(crate) async fn fill_held_link(&self, src: &Corpus, read: ReadLink) -> Result<()> {
        let url = src
            .source_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
            .ok_or_else(|| Error::Validation("a held link with no link".into()))?;
        let title_given = src.metadata["fetch"]["title_given"]
            .as_bool()
            .unwrap_or(false);
        let note = src.metadata["note"].as_str().map(str::to_string);
        match read {
            ReadLink::Page { markdown, title } => {
                let sig = crate::store::shingle::signature(&markdown);
                self.store.set_read_text(&src.id, &markdown, sig).await?;
                if !title_given && let Some(t) = title.as_deref() {
                    self.store.set_title_hint(&src.id, t).await?;
                }
                let mut meta = src.metadata.clone();
                meta["fetch"] = serde_json::json!({ "fetched_at": now() });
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.reprocess(&src.id, Stage::Synthesize).await?;
                tracing::info!(corpus_id = %src.id, url = %url, "a held link was read");
            }
            read @ (ReadLink::Pdf(_) | ReadLink::Image(_)) => {
                let lang = crate::infer::lang::of_corpus(&src.metadata);
                let title = title_given.then(|| src.title_hint.clone()).flatten();
                let out = self.ingest_read(&url, read, title, note, lang).await?;
                self.delete_corpus(&src.id).await?;
                tracing::info!(corpus_id = %src.id, now = %out.id, url = %url, "a held link was read into a file capture");
            }
        }
        Ok(())
    }

    /// Left searchable as the link with the last reason; `reprocess(Fetch)` tries again.
    pub(crate) async fn give_up_on_link(&self, corpus_id: &str, why: &str) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let mut meta = src.metadata.clone();
        meta["fetch"]["pending"] = serde_json::Value::Bool(false);
        meta["fetch"]["error"] = serde_json::Value::String(why.to_string());
        self.store.set_corpus_metadata(corpus_id, &meta).await
    }

    /// Only single-window corpora are judged, so a long forced reminder would arm nothing; refused
    /// before the insert.
    fn refuse_a_reminder_too_large_to_judge(&self, c: &Capture) -> Result<()> {
        let remind = crate::core::moments::Intent::Remind;
        if c.metadata["intent"].as_str() != Some(remind.as_str()) {
            return Ok(());
        }
        // The embed-only floor: no model can read a reminder.
        if !self.synthesizes() {
            return Err(Error::Validation(NO_SYNTHESIZER_REMINDER.into()));
        }
        let lang = crate::infer::lang::of_corpus(&c.metadata);
        // No budget at all: nothing can be judged in one pass.
        let Some(budget) = crate::jobs::synthesize::segment_budget(self, lang) else {
            return Err(Error::Validation(
                crate::jobs::synthesize::no_budget_reason(self, lang),
            ));
        };
        let windows = crate::infer::split::split_into_segments(&c.text, &self.counter, budget);
        if windows.len() <= 1 {
            return Ok(());
        }
        Err(Error::Validation(format!(
            "a reminder is read in one pass, and this text is {} tokens against a window of \
             {budget} — it would be split into {} and never judged. Capture it with `engram -c` \
             and set the reminder on a sentence.",
            self.counter.count(&c.text),
            windows.len(),
        )))
    }

    /// The same thing, for a door that also knows where the text was read.
    pub async fn ingest_capture(&self, c: Capture) -> Result<IngestOutcome> {
        let out = self.ingest_capture_quiet(c).await;
        self.live.changed();
        out
    }

    async fn ingest_capture_quiet(&self, c: Capture) -> Result<IngestOutcome> {
        let text = c.text.as_str();
        let origin = c.origin.as_str();
        let title_hint = c.title_hint.as_deref();
        if text.trim().is_empty() {
            return Err(Error::Validation("text is empty".into()));
        }
        self.refuse_a_reminder_too_large_to_judge(&c)?;

        let forced_remind =
            c.metadata["intent"].as_str() == Some(crate::core::moments::Intent::Remind.as_str());

        if let Some(existing) = self.store.find_by_hash(&content_hash(text)).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate ingest, returning existing source");
            // The same bytes again with "remind me": no new text, but the request must still arm.
            if forced_remind {
                self.ask_the_judged_read_for_a_reminder(&existing.id)
                    .await?;
            }
            self.close_the_gap_this_answered(&existing.id, &c.metadata);
            return Ok(IngestOutcome::existing(&existing));
        }

        // One signature answers the near-duplicate scan and becomes the stored column.
        let sig = crate::store::shingle::signature(text);
        let near = self
            .store
            .find_near_duplicate(&sig, self.consolidate.near_dupe_min)
            .await?;

        // Queued either way; a near-duplicate is only flagged, and pair judging reconciles
        // downstream.
        let followup = match &near {
            // A reminder repeated is a reminder, not a copy.
            Some(n) if !forced_remind => Followup::Flag {
                of: n.corpus_id.clone(),
                similarity: n.similarity,
            },
            _ => Followup::Queue(Stage::Synthesize),
        };
        let src = match self
            .store
            .insert_corpus_with_signature(
                text,
                origin,
                title_hint,
                sig,
                c.source_url.as_deref(),
                &c.metadata,
                followup,
            )
            .await?
        {
            Insertion::Created(src) => src,
            // A concurrent capture of the same bytes won the insert during the scan; the winner
            // queued it.
            Insertion::Existing(existing) => {
                tracing::info!(corpus_id = %existing.id, "concurrent duplicate ingest, returning the stored source");
                // The race's loser still carries "remind me", as the `find_by_hash` branch honours.
                if forced_remind {
                    self.ask_the_judged_read_for_a_reminder(&existing.id)
                        .await?;
                }
                self.close_the_gap_this_answered(&existing.id, &c.metadata);
                return Ok(IngestOutcome::existing(&existing));
            }
        };

        match &near {
            Some(n) => tracing::info!(
                corpus_id = %src.id,
                near = %n.corpus_id,
                similarity = n.similarity,
                flagged = !forced_remind,
                "capture looks like an existing corpus"
            ),
            None => tracing::info!(corpus_id = %src.id, origin, bytes = text.len(), "ingested"),
        }

        self.attach_note_artifact(&src.id, c.metadata["note"].as_str())
            .await;

        Ok(IngestOutcome {
            id: src.id,
            status: src.status,
            duplicate: false,
            // A forced reminder is not flagged, so it is not reported as one.
            near_duplicate: near.filter(|_| !forced_remind),
            link_unread: None,
        })
    }

    /// Queued for synthesis and flagged beside what it resembles; never held.
    pub(crate) async fn park_or_queue(
        &self,
        corpus_id: &str,
        near: Option<&crate::store::corpora::NearDuplicate>,
    ) -> Result<()> {
        let followup = match near {
            Some(n) => crate::store::corpora::Followup::Flag {
                of: n.corpus_id.clone(),
                similarity: n.similarity,
            },
            None => crate::store::corpora::Followup::Queue(Stage::Synthesize),
        };
        // One transaction: the status and the job it implies cannot be written apart.
        self.store.apply_followup(corpus_id, followup).await?;
        if let Some(n) = near {
            tracing::info!(
                corpus_id,
                near = %n.corpus_id,
                similarity = n.similarity,
                "looks like an existing corpus; flagged, and read as usual"
            );
        }
        Ok(())
    }

    /// The note as an artifact, since embedding never reads metadata. No span and no segment, so it
    /// sorts first; `Provenance::Note` keeps the sweeps of window-less rows off it. Armed here: a
    /// scan without text never settles.
    async fn attach_note_artifact(&self, corpus_id: &str, note: Option<&str>) {
        if let Err(e) = self.write_note_artifact(corpus_id, note).await {
            tracing::error!(corpus_id, error = %e, "capture stored, but its note was not");
        }
    }

    async fn write_note_artifact(&self, corpus_id: &str, note: Option<&str>) -> Result<()> {
        let Some(text) = note.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        self.store
            .insert_artifacts_with_provenance(
                corpus_id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: text.to_string(),
                    corpus_span: None,
                    // A heading is something a document gave. This had none.
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
                crate::store::artifacts::Provenance::Note,
            )
            .await?;
        self.store
            .rearm_idle_seq(Stage::Embed, "corpus", corpus_id, 0)
            .await?;
        Ok(())
    }

    /// Store a file by what its bytes say it is: a PDF, an image, or UTF-8 text; anything else is
    /// refused. `origin` is the caller's: `/mcp`, `share` and `web` differ in a way a person later
    /// cares about.
    pub async fn ingest_file(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
        title: Option<String>,
        note: Option<String>,
        origin: &str,
        lang: crate::infer::lang::Lang,
    ) -> Result<IngestOutcome> {
        if bytes.starts_with(b"%PDF-") {
            return self
                .ingest_pdf(PdfCapture {
                    bytes,
                    filename,
                    title_hint: title,
                    note,
                    lang,
                })
                .await;
        }
        if image::guess_format(&bytes).is_ok() {
            return self
                .ingest_image(ImageCapture {
                    bytes,
                    filename,
                    title_hint: title,
                    note,
                    lang,
                })
                .await;
        }
        let size = bytes.len();
        if size > MAX_TEXT_BYTES {
            return Err(Error::Validation(format!(
                "that file is over the {} MB limit for a text capture",
                MAX_TEXT_BYTES / (1024 * 1024)
            )));
        }
        let text = decode_text(bytes).map_err(|e| {
            Error::Validation(match e {
                NotText::Undecodable => {
                    "that file is neither a PDF, an image nor UTF-8 text — nothing here reads it"
                }
                NotText::Nul => NotText::NUL_MESSAGE,
            }
            .into())
        })?;
        self.ingest_capture(
            Capture::new(text, origin)
                .with_lang(lang)
                .with_title(title)
                .with_note(note)
                .with_file(filename.as_deref(), size, "text/plain"),
        )
        .await
    }

    pub async fn ingest_pdf(&self, c: PdfCapture) -> Result<IngestOutcome> {
        self.ingest_pdf_from(c, None).await
    }

    /// The link is provenance on the corpus, as for a fetched page.
    pub async fn ingest_pdf_from(
        &self,
        c: PdfCapture,
        source_url: Option<String>,
    ) -> Result<IngestOutcome> {
        let PdfCapture {
            bytes,
            filename,
            title_hint,
            note,
            lang,
        } = c;
        // Here, not at each door: routes serving both kinds are layered at the larger ceiling.
        if bytes.len() > self.capture.pdf_max_bytes {
            return Err(Error::Validation(format!(
                "that PDF is over the {} MB limit for a PDF capture",
                self.capture.pdf_max_bytes / (1024 * 1024)
            )));
        }
        // Hashed first: the same PDF twice costs one SHA-256, not an extraction.
        let hash = content_hash(&bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate PDF, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }

        // The image door's `file` namespace; no page count in this build.
        let mut file = serde_json::json!({
            "size": bytes.len(),
            "mime": "application/pdf",
        });
        if let Some(n) = filename.as_deref() {
            file["name"] = serde_json::Value::String(n.to_string());
        }
        let mut metadata = serde_json::json!({ "file": file });
        // PDFs and images bypass `Capture`, so the `Capture::with_lang` key must be written here
        // too.
        metadata["lang"] = serde_json::json!(lang.tag());
        let note = clean_note(note);
        if let Some(n) = &note {
            metadata["note"] = serde_json::json!(n);
        }

        let inserted = self
            .store
            .insert_attached_corpus(
                &hash,
                ORIGIN_PDF,
                title_hint.as_deref(),
                source_url.as_deref(),
                &metadata,
                crate::store::corpora::Reading::EXTRACTION,
                &crate::store::attachments::NewFile {
                    kind: "pdf",
                    mime: "application/pdf",
                    filename: filename.as_deref(),
                    bytes: &bytes,
                    preview: &[],
                    width: None,
                    height: None,
                },
            )
            .await?;
        Ok(match inserted {
            // A file already in the base keeps its note; re-uploads do not stack captions.
            Insertion::Existing(c) => IngestOutcome::existing(&c),
            Insertion::Created(c) => {
                self.attach_note_artifact(&c.id, note.as_deref()).await;
                IngestOutcome {
                    id: c.id,
                    status: c.status,
                    duplicate: false,
                    near_duplicate: None,
                    link_unread: None,
                }
            }
        })
    }

    pub async fn ingest_image(&self, c: ImageCapture) -> Result<IngestOutcome> {
        self.ingest_image_from(c, None).await
    }

    pub async fn ingest_image_from(
        &self,
        c: ImageCapture,
        source_url: Option<String>,
    ) -> Result<IngestOutcome> {
        // No vision role is not a refusal: stored, and `Stage::Describe` waits for the role.
        if self.describer.is_none() {
            tracing::info!("no vision role: storing the image to be read when one is configured");
        }
        let ImageCapture {
            bytes,
            filename,
            title_hint,
            note,
            lang,
        } = c;
        // Before the hash and the decode permit, so an oversize photo costs neither.
        if bytes.len() > self.capture.image_max_bytes {
            return Err(Error::Validation(format!(
                "that image is over the {} MB limit for an image capture",
                self.capture.image_max_bytes / (1024 * 1024)
            )));
        }
        // Hashed before decoding: a photo sent twice costs one SHA-256.
        let hash = content_hash(&bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate image, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }
        // Decoding is synchronous pixel work, so it runs blocking, under a permit taken before it
        // starts: the blocking pool is 512 deep. See `image::MAX_CONCURRENT_DECODES`.
        let edge = self.capture.image_preview_edge;
        let permit = self
            .decodes
            .acquire()
            .await
            .map_err(|_| Error::Internal("the decode permit closed".into()))?;
        let decoded = tokio::task::spawn_blocking(move || {
            let prepared = super::image::prepare(&bytes, edge)?;
            Ok::<_, Error>((bytes, prepared))
        })
        .await
        .map_err(|e| Error::Internal(format!("image preparation did not finish: {e}")))?;
        drop(permit);
        let (bytes, prepared) = decoded?;

        let mut metadata = serde_json::json!({
            "file": super::image::file_facts(filename.as_deref(), bytes.len(), &prepared),
        });
        if prepared.exif.as_object().is_some_and(|o| !o.is_empty()) {
            metadata["exif"] = prepared.exif.clone();
        }
        // PDFs and images bypass `Capture`, so the `Capture::with_lang` key must be written here
        // too.
        metadata["lang"] = serde_json::json!(lang.tag());
        let note = clean_note(note);
        if let Some(n) = &note {
            metadata["note"] = serde_json::Value::String(n.clone());
        }
        // A filename is not a name; seeding the title from `photo.jpg` would disarm the naming
        // stage.

        let attachment = crate::store::attachments::NewFile {
            kind: "image",
            mime: prepared.mime,
            filename: filename.as_deref(),
            bytes: &bytes,
            preview: &prepared.preview_jpeg,
            width: Some(prepared.width as i64),
            height: Some(prepared.height as i64),
        };
        let src = match self
            .store
            .insert_attached_corpus(
                &hash,
                ORIGIN_IMAGE,
                title_hint.as_deref(),
                source_url.as_deref(),
                &metadata,
                crate::store::corpora::Reading::VISION,
                &attachment,
            )
            .await?
        {
            Insertion::Created(src) => src,
            Insertion::Existing(existing) => {
                return Ok(IngestOutcome::existing(&existing));
            }
        };
        tracing::info!(
            corpus_id = %src.id,
            bytes = bytes.len(),
            mime = prepared.mime,
            "image captured; queued for reading"
        );
        self.attach_note_artifact(&src.id, note.as_deref()).await;
        Ok(IngestOutcome {
            id: src.id,
            status: CorpusStatus::Describing,
            duplicate: false,
            near_duplicate: None,
            link_unread: None,
        })
    }

    /// The one way out of `needs_review`, which nothing writes any more.
    pub(crate) async fn release_parked(&self, corpus_id: &str) -> Result<()> {
        // Status first, then the job: an interruption leaves a `raw` corpus with no unit, which
        // reconcile finds.
        self.store
            .set_corpus_status(corpus_id, CorpusStatus::Raw)
            .await?;
        self.store
            .enqueue(Stage::Synthesize, "corpus", corpus_id)
            .await?;
        Ok(())
    }

    /// Payload flag first, then the row: a partial failure leaves it on Ops with its restore
    /// button.
    pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        self.unsupersede_locked(artifact_id).await
    }

    /// Entered with `lifecycle_lock` held; `reactivate` routes here so it is not locked twice.
    async fn unsupersede_locked(&self, artifact_id: &str) -> Result<()> {
        // Read before clearing: afterwards nothing says who was hiding it.
        let winner = self.store.get_artifact(artifact_id).await?.superseded_by;
        // A buried artifact comes back through the graveyard or not at all; its text and point are
        // gone.
        if !self.exhume_locked(artifact_id, None).await? {
            // Marked first: this direction writes the payload first, and a crash would leave
            // unannounced drift.
            self.store.mark_lifecycle_dirty(artifact_id).await?;
            self.vectors
                .set_lifecycle(artifact_id, ArtifactStatus::Active, None)
                .await?;
            self.store.set_superseded_by(artifact_id, None).await?;
            // Cleared only after both stores agree.
            self.store
                .clear_lifecycle_dirty(std::slice::from_ref(&artifact_id.to_string()))
                .await?;
        }
        // Recorded on the lineage, or the unfinished-merge repair re-hides it every tick.
        if let Some(w) = winner
            && let Ok(wc) = self.store.get_artifact(&w).await
            && wc.provenance == crate::store::artifacts::Provenance::Merged
        {
            self.store.mark_source_restored(artifact_id).await?;
        }
        self.reopen_the_pairs_that_were_waiting_on(artifact_id)
            .await;
        // `uncovered` only sees active artifacts, so a restored artifact must re-arm its reminder.
        // Best-effort: the restore has committed, and the background sweep re-arms on its own tick.
        if let Err(e) = self.store.rearm_remind().await {
            tracing::warn!(artifact_id, error = %e, "could not re-arm Remind after a restore");
        }
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }

    /// Reopen the `Stale` pairs its supersession settled; `record_pair` is `INSERT OR IGNORE` and
    /// would file nothing. Logged, not returned: the restore has already happened.
    async fn reopen_the_pairs_that_were_waiting_on(&self, artifact_id: &str) {
        match self.store.reopen_stale_pairs(artifact_id).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                pairs = n,
                artifact_id,
                "reopened the pairs it had taken with it"
            ),
            Err(e) => {
                tracing::warn!(artifact_id, error = %e, "could not reopen the artifact's pairs")
            }
        }
    }

    /// Undo a promotion and mark the window `no_promote`, or the passages' activation would promote
    /// it again. Only a re-split clears the mark. Copied links and activation stay.
    pub async fn undo_promotion(&self, corpus_id: &str, idx: i64) -> Result<()> {
        use crate::store::artifacts::Provenance;
        let rows = self.store.artifacts_for_segment(corpus_id, idx).await?;
        for c in rows
            .iter()
            .filter(|c| c.provenance == Provenance::Passage && c.superseded_by.is_some())
        {
            self.unsupersede(&c.id).await?;
        }
        for c in rows
            .iter()
            .filter(|c| c.provenance != Provenance::Passage && c.in_results())
        {
            self.deprecate(&c.id).await?;
        }
        self.store
            .set_segment_state(
                corpus_id,
                idx,
                crate::store::segments::SegmentState::Verbatim,
                None,
            )
            .await?;
        self.store.set_segment_no_promote(corpus_id, idx).await?;
        tracing::info!(corpus_id, window = idx, "promotion undone");
        Ok(())
    }

    /// Re-point an already-superseded artifact, so a merge never leaves an `A -> B -> C` chain. Row
    /// before payload.
    pub async fn repoint_supersession(&self, artifact_id: &str, winner_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        let winner = self.store.get_artifact(winner_id).await?;
        if !winner.in_results() {
            return Err(Error::Validation(format!(
                "cannot re-point {artifact_id}: {winner_id} is not active"
            )));
        }
        self.store
            .set_superseded_by(artifact_id, Some(winner_id))
            .await?;
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Superseded, Some(winner_id))
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&artifact_id.to_string()))
            .await?;
        tracing::info!(artifact_id, winner_id, "re-pointed a supersession");
        Ok(())
    }

    /// Row before payload, as the sweep's auto-supersede does, so a partial failure is visible on
    /// Ops.
    pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
        self.supersede_with(loser_id, winner_id, None).await
    }

    /// The journal row rides the hiding's transaction.
    pub async fn supersede_with(
        &self,
        loser_id: &str,
        winner_id: &str,
        journal: Option<crate::store::actions::NewAction>,
    ) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // Neither side may be retired: `set_superseded_by` overwrites status, and a deprecated
        // winner hides the answer.
        for (id, role) in [(loser_id, "loser"), (winner_id, "winner")] {
            let status = self.store.get_artifact(id).await?.status;
            if status != ArtifactStatus::Active {
                return Err(Error::Validation(format!(
                    "cannot supersede: {role} {id} is {}",
                    status.as_str()
                )));
            }
        }
        self.store
            .set_superseded_by_with(loser_id, Some(winner_id), journal.as_ref())
            .await?;
        self.vectors
            .set_lifecycle(loser_id, ArtifactStatus::Superseded, Some(winner_id))
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&loser_id.to_string()))
            .await?;
        // Logged, not returned: the supersession has happened; `follow_supersessions` moves stale
        // pairs next tick.
        match self.store.follow_supersession(loser_id, winner_id).await {
            Ok(f) if f.settled() == 0 => {}
            Ok(f) => tracing::info!(
                moved = f.moved,
                staled = f.staled,
                winner_id,
                "settled the open pairs the superseded artifact left"
            ),
            Err(e) => {
                tracing::warn!(loser_id, winner_id, error = %e, "could not move the loser's open pairs")
            }
        }
        // Best-effort likewise; then `rearm_remind`, since moved rows are visible again on the
        // winner.
        match self.store.carry_moments(loser_id, winner_id).await {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(
                    moved = n,
                    loser_id,
                    winner_id,
                    "carried the superseded artifact's open moments to its winner"
                );
                if let Err(e) = self.store.rearm_remind().await {
                    tracing::warn!(error = %e, "could not re-arm the reminder unit after a supersession");
                }
            }
            Err(e) => {
                tracing::warn!(loser_id, winner_id, error = %e, "could not carry the loser's moments")
            }
        }
        tracing::info!(loser_id, winner_id, "superseded an artifact");
        Ok(())
    }

    /// Row before payload: SQLite is the source of truth, so drift repair finishes a partial
    /// failure.
    pub async fn deprecate(&self, id: &str) -> Result<()> {
        self.deprecate_with(id, None).await
    }

    /// `deprecate`, journaling what a corpus job did; see `supersede_with`.
    pub async fn deprecate_with(
        &self,
        id: &str,
        journal: Option<crate::store::actions::NewAction>,
    ) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // A superseded artifact stays superseded: deprecating it too yields a state with no undo
        // button.
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return Err(Error::Validation(format!(
                "cannot deprecate: {id} is already hidden in favour of another artifact; \
                 reactivate it first"
            )));
        }
        self.store
            .set_artifact_status_with(id, ArtifactStatus::Deprecated, journal.as_ref())
            .await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Deprecated, None)
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        tracing::info!(artifact_id = id, "deprecated an artifact");
        Ok(())
    }

    /// Entered with `lifecycle_lock` held. `false` where it was not reaped or the grave is empty.
    /// Text restored, then row, then `Embed` rebuilds the point; `status` is set before the `Embed`
    /// is queued.
    async fn exhume_locked(&self, id: &str, status: Option<ArtifactStatus>) -> Result<bool> {
        if self.store.get_artifact(id).await?.reaped_at.is_none() || !self.store.exhume(id).await? {
            return Ok(false);
        }
        if let Some(s) = status {
            self.store.set_artifact_status(id, s).await?;
        }
        self.store.set_superseded_by(id, None).await?;
        // After both setters, which each raise `lifecycle_dirty` again; the point does not exist
        // until `Embed` lands.
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        self.store.enqueue(Stage::Embed, "artifact", id).await?;
        tracing::info!(artifact_id = id, "exhumed a reaped artifact");
        Ok(true)
    }

    /// A superseded artifact goes through `unsupersede`. Payload before row, so the intermediate
    /// state is visible.
    pub async fn reactivate(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        let art = self.store.get_artifact(id).await?;
        // Read before `exhume_locked` clears it: a buried and superseded artifact still needs the
        // lineage mark.
        let winner = art.superseded_by.clone();
        if self.exhume_locked(id, Some(ArtifactStatus::Active)).await? {
            if let Some(w) = winner
                && let Ok(wc) = self.store.get_artifact(&w).await
                && wc.provenance == crate::store::artifacts::Provenance::Merged
            {
                self.store.mark_source_restored(id).await?;
            }
            self.reopen_the_pairs_that_were_waiting_on(id).await;
            // Best-effort: the exhume has committed.
            if let Err(e) = self.store.rearm_remind().await {
                tracing::warn!(artifact_id = id, error = %e, "could not re-arm Remind after an exhume");
            }
            return Ok(());
        }
        if art.superseded_by.is_some() {
            return self.unsupersede_locked(id).await;
        }
        // As in `unsupersede`: payload first, so the marker goes first.
        self.store.mark_lifecycle_dirty(id).await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Active, None)
            .await?;
        self.store
            .set_artifact_status(id, ArtifactStatus::Active)
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        self.reopen_the_pairs_that_were_waiting_on(id).await;
        // Best-effort: the reactivation is already written.
        if let Err(e) = self.store.rearm_remind().await {
            tracing::warn!(artifact_id = id, error = %e, "could not re-arm Remind after a reactivation");
        }
        tracing::info!(artifact_id = id, "reactivated an artifact");
        Ok(())
    }

    /// Best-effort; artifacts out of results are skipped.
    pub async fn confirm_cited(&self, ids: &[String]) {
        for id in ids {
            match self.store.get_artifact(id).await {
                Ok(c) if c.in_results() => {
                    if let Err(e) = self.verify(id).await {
                        tracing::warn!(artifact_id = %id, error = %e, "could not stamp a confirmed artifact");
                    }
                }
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => {
                    tracing::warn!(artifact_id = %id, error = %e, "could not read a confirmed artifact")
                }
            }
        }
    }

    /// A right answer also confirms the notes it drew on; wrong or empty ones say nothing about
    /// them.
    pub async fn judge_ask(&self, id: &str, verdict: crate::store::asks::AskVerdict) -> Result<()> {
        self.store.judge_ask(id, verdict).await?;
        if verdict == crate::store::asks::AskVerdict::Right {
            // Best-effort past this point: the verdict is written.
            match self.store.used_citations(id).await {
                Ok(cited) => self.confirm_cited(&cited).await,
                Err(e) => tracing::warn!(
                    ask_id = id,
                    error = %e,
                    "could not read what a right answer cited; its notes are not confirmed"
                ),
            }
        }
        Ok(())
    }

    /// Stamp an artifact confirmed now; also zeroes `hit_count`, which counts since the last
    /// verification. A failed vector write rolls the row back: drift repair never sees an
    /// active-on-both mismatch.
    pub async fn verify(&self, id: &str) -> Result<()> {
        let at = now();
        let previous = self.store.get_artifact(id).await?.last_verified_at;
        self.store.set_last_verified_at(id, at).await?;
        if let Err(e) = self.vectors.set_last_verified_at(id, at, true).await {
            // A row that never had a stamp has nothing to roll back to.
            if let Some(previous) = previous
                && let Err(undo) = self.store.set_last_verified_at(id, previous).await
            {
                tracing::warn!(
                    artifact_id = id,
                    error = %undo,
                    "could not undo the verification stamp; sqlite now claims a \
                     verification the vector store did not record"
                );
            }
            return Err(e);
        }
        tracing::info!(artifact_id = id, "verified an artifact");
        Ok(())
    }

    /// Rows `embedded` with no point are re-armed; points with no row are only reported (SQLite is
    /// the truth). Points are read before rows, so an artifact captured meanwhile is never mistaken
    /// for an orphan.
    pub async fn heal_store_drift(&self) -> Result<()> {
        let points = self.vectors.all_artifact_ids().await?;
        let rows = self.store.list_all_artifact_ids().await?;
        let embedded = self.store.list_embedded_artifact_ids().await?;
        let drift = store_drift(&rows, &embedded, &points);

        let orphan_points = drift.orphan_points.len();
        if orphan_points > 0 {
            tracing::warn!(
                orphan_points,
                "the vector store holds artifacts SQLite does not; restore both stores from the same snapshot"
            );
        }

        let mut requeued = 0usize;
        for id in &drift.missing_points {
            // Idle-only: `enqueue` would reset a failing unit's attempts every tick, and the
            // backoff is the breaker.
            self.store
                .rearm_idle_seq(Stage::Embed, "artifact", id, 0)
                .await?;
            requeued += 1;
        }
        if requeued > 0 {
            tracing::info!(
                requeued,
                "artifacts SQLite calls embedded had no point; re-queued"
            );
        }
        Ok(())
    }

    /// Takes an artifact out of every read, keeping it restorable for 30 days.
    pub async fn trash_artifact(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        self.store.trash_artifact(id, crate::store::now()).await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Trashed, None)
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        drop(_guard);
        // Whatever this was hiding is freed, as a delete frees it.
        self.heal_dangling_supersessions().await?;
        tracing::info!(artifact_id = id, "moved an artifact to trash");
        Ok(())
    }

    /// Takes an artifact out of Trash with the status and supersession it had.
    pub async fn restore_artifact(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        let status = self.store.trashed_from(id).await?.ok_or(Error::NotFound)?;
        let superseded_by = self.store.get_artifact(id).await?.superseded_by;
        // Payload first, as every reveal does: a crash in between leaves it visible, not lost.
        self.store.mark_lifecycle_dirty(id).await?;
        self.vectors
            .set_lifecycle(id, status, superseded_by.as_deref())
            .await?;
        self.store.untrash_artifact(id).await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        tracing::info!(artifact_id = id, "restored an artifact from trash");
        Ok(())
    }

    /// Deletes for good what has been in Trash longer than `older_than_secs`.
    pub async fn purge_trash(&self, older_than_secs: i64) -> Result<u64> {
        let mut n = 0;
        for id in self
            .store
            .trashed_before(crate::store::now() - older_than_secs)
            .await?
        {
            self.delete_artifact(&id).await?;
            n += 1;
        }
        Ok(n)
    }

    /// The point goes first, so an interrupted delete is re-embedded by `heal_store_drift` and can
    /// be retried.
    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        self.store.get_artifact(id).await?;
        self.vectors
            .delete_artifacts(std::slice::from_ref(&id.to_string()))
            .await?;
        self.store.delete_artifact(id).await?;
        // This may have been the keeper hiding another artifact.
        self.heal_dangling_supersessions().await?;
        tracing::info!(artifact_id = id, "deleted an artifact");
        Ok(())
    }

    /// Un-hide anything superseded by a since-deleted artifact; runs after every deletion.
    /// Payload before row, as `unsupersede`; one failure does not stop the rest.
    pub(crate) async fn heal_dangling_supersessions(&self) -> Result<()> {
        // Under the lifecycle lock: a payload-first reveal interleaved with the repair hides an
        // artifact unmarked.
        let _guard = self.lifecycle_lock.lock().await;
        let mut first_err = None;
        for id in self.store.dangling_superseded().await? {
            // Marked before the payload write; a failed write leaves the marker as findable drift.
            self.store.mark_lifecycle_dirty(&id).await?;
            if let Err(e) = self
                .vectors
                .set_lifecycle(&id, ArtifactStatus::Active, None)
                .await
            {
                tracing::warn!(
                    artifact_id = %id,
                    error = %e,
                    "could not clear the hidden flag; the artifact stays listed on Ops"
                );
                first_err.get_or_insert(e);
                continue;
            }
            self.store.set_superseded_by(&id, None).await?;
            // `set_superseded_by` raises the marker; both stores already agree here, so clear it.
            self.store
                .clear_lifecycle_dirty(std::slice::from_ref(&id))
                .await?;
            tracing::info!(
                artifact_id = %id,
                "restored an artifact whose surviving copy was deleted"
            );
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Vectors first: an orphaned row is invisible, but an orphaned vector is
    /// still returned by search.
    pub async fn delete_corpus(&self, id: &str) -> Result<()> {
        self.store.get_corpus(id).await?;
        self.vectors.delete_by_corpus(id).await?;
        self.store.delete_corpus(id).await?;
        self.heal_dangling_supersessions().await?;
        tracing::info!(corpus_id = %id, "deleted source and its vectors");
        Ok(())
    }

    /// What a rerun from `raw_text` has to forget first; shared by re-segment and re-read.
    async fn forget_derived_work(&self, id: &str) -> Result<()> {
        // Except the note, which no rerun can reproduce; it is re-armed for a new vector here.
        self.vectors.delete_by_corpus(id).await?;
        for c in self.store.artifacts_for_corpus(id).await? {
            if c.provenance == crate::store::artifacts::Provenance::Note {
                continue;
            }
            self.store.delete_artifact(&c.id).await?;
        }
        self.store.reset_embed_state(id).await?;
        self.store
            .rearm_idle_seq(Stage::Embed, "corpus", id, 0)
            .await?;
        // Window rows are the segment job's memory; left behind, the rerun segments nothing.
        self.store.clear_segments(id).await?;
        // Reconcile reads no coverage as "settle never ran"; a stale value would hide a stuck
        // rerun.
        self.store.set_corpus_coverage(id, None).await?;
        // Window units outlive their rows and would carry their attempts into the rerun.
        self.store.delete_window_jobs(id).await?;
        // The title unit is armed once per corpus; an explicit reprocess may try again.
        self.store.delete_job(Stage::Title, id).await?;
        // Reprocessing a parked capture lifts the park, or real work sits on the review queue.
        self.store.set_near_dupe(id, None, None).await?;
        Ok(())
    }

    /// Close the gap this capture answered when its bytes were already stored and no `cover` runs.
    /// Best-effort, on the background handle.
    fn close_the_gap_this_answered(&self, corpus_id: &str, metadata: &serde_json::Value) {
        let Some(event) = typed_from(metadata).map(str::to_string) else {
            return;
        };
        let core = self.clone();
        let id = corpus_id.to_string();
        self.background.spawn(async move {
            if let Err(e) = crate::jobs::gaps::cover_answering(&core, &id, Some(event)).await {
                tracing::warn!(corpus_id = %id, error = %e, "could not close the gap a stored capture answered");
            }
        });
    }

    async fn ask_the_judged_read_for_a_reminder(&self, corpus_id: &str) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let mut meta = src.metadata.clone();
        let intent = crate::core::moments::Intent::Remind;
        meta["intent"] = serde_json::Value::String(intent.as_str().to_string());
        crate::core::moments::allow_intent(&mut meta, intent);
        self.store.set_corpus_metadata(corpus_id, &meta).await?;
        let windows = self.store.segments_for_corpus(corpus_id).await?;
        // Nothing has read it yet; the judged read still to come will see the intent.
        if windows.is_empty() {
            tracing::info!(
                corpus_id,
                "the capture has not been read yet; the reminder rides on its first read"
            );
            return Ok(());
        }
        if windows.len() != 1 {
            tracing::info!(
                corpus_id,
                windows = windows.len(),
                "the stored capture is not read in one pass; no reminder was armed on it"
            );
            return Ok(());
        }
        let idx = windows[0].idx;
        // An undone promotion's mark: re-reading this window would promote it again.
        if self.store.segment_no_promote(corpus_id, idx).await? {
            tracing::info!(
                corpus_id,
                window = idx,
                "this window's promotion was undone; the reminder is not read from it again"
            );
            return Ok(());
        }
        self.store.reset_segment(corpus_id, idx, true).await?;
        self.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, idx),
                idx,
            )
            .await
    }

    /// The window `set_reminder(id, true)` would hand back, asked by the band too so the undo keeps
    /// its promise.
    async fn reminder_reread(&self, artifact_id: &str) -> Result<Option<i64>> {
        // No `[infer.synthesize]`, no judged read.
        if !self.synthesizes() {
            return Ok(None);
        }
        let art = self.store.get_artifact(artifact_id).await?;
        // A merge belongs to no corpus, so there is no note to read again.
        let Some(cid) = art.corpus_id.as_deref() else {
            return Ok(None);
        };
        // Only a single-window capture is judged; zero windows has nothing to hand back either.
        let windows = self.store.segments_for_corpus(cid).await?.len();
        if windows != 1 {
            tracing::info!(
                corpus_id = cid,
                windows,
                "a reminder is read in one pass; this capture has no single window to re-read"
            );
            return Ok(None);
        }
        // An artifact with no window cannot be handed back.
        let Some(idx) = art.segment_idx else {
            tracing::info!(
                artifact_id,
                "artifact belongs to no window; there is nothing to re-read"
            );
            return Ok(None);
        };
        // Read before any metadata is written, so a refusal changes nothing.
        if self.store.segment_no_promote(cid, idx).await? {
            tracing::info!(
                corpus_id = cid,
                window = idx,
                "this window's promotion was undone; it is not read again for a reminder"
            );
            return Ok(None);
        }
        Ok(Some(idx))
    }

    /// What the band asks before offering the undo.
    pub async fn can_be_a_reminder(&self, artifact_id: &str) -> Result<bool> {
        Ok(self.reminder_reread(artifact_id).await?.is_some())
    }

    /// `off` deletes the rows the stage owns and records a durable refusal; `on` withdraws it and
    /// re-reads. Returns whether the refusal removed anything.
    pub async fn set_reminder(&self, artifact_id: &str, on: bool) -> Result<bool> {
        let art = self.store.get_artifact(artifact_id).await?;
        let intent = crate::core::moments::Intent::Remind;
        let Some(cid) = art.corpus_id.as_deref() else {
            // A merge has no corpus, but `carry_moments` moves due rows onto it: `off` still
            // deletes them.
            if on {
                return Ok(false);
            }
            let gone = self.store.delete_refused_due(artifact_id).await?;
            self.store.rearm_remind().await?;
            return Ok(gone > 0);
        };
        let src = self.store.get_corpus(cid).await?;
        let mut meta = src.metadata.clone();
        if on {
            // One question, shared with the band's undo; see `reminder_reread`.
            let Some(idx) = self.reminder_reread(artifact_id).await? else {
                return Ok(false);
            };
            crate::core::moments::allow_intent(&mut meta, intent);
            self.store.set_corpus_metadata(cid, &meta).await?;
            // Armed as every automatic arming is: `enqueue_seq` would rewrite a `running` row and
            // double the work.
            self.store.reset_segment(cid, idx, true).await?;
            self.store
                .rearm_idle_seq(
                    Stage::SegmentWindow,
                    "segment",
                    &crate::jobs::window::unit_target(cid, idx),
                    idx,
                )
                .await?;
            return Ok(true);
        }
        crate::core::moments::refuse_intent(&mut meta, intent);
        self.store.set_corpus_metadata(cid, &meta).await?;
        let gone = self.store.delete_refused_due(artifact_id).await?;
        self.store.rearm_remind().await?;
        Ok(gone > 0)
    }

    pub async fn reprocess(&self, id: &str, stage: Stage) -> Result<()> {
        let src = self.store.get_corpus(id).await?;
        match stage {
            Stage::Synthesize => {
                // An image whose read has not landed has no `raw_text` to re-segment.
                if src.status == CorpusStatus::Describing
                    || (src.origin == ORIGIN_IMAGE && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this image has not been read yet — re-read it instead".into(),
                    ));
                }
                // Likewise a PDF whose extraction has not landed.
                if src.status == CorpusStatus::Extracting
                    || (src.origin == ORIGIN_PDF && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this PDF has not been extracted yet — re-extract it instead".into(),
                    ));
                }
                self.forget_derived_work(&src.id).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
                // The deleted artifacts may have been the surviving half of a pair.
                self.heal_dangling_supersessions().await?;
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
            }
            Stage::Embed => {
                self.store.reset_embed_state(&src.id).await?;
                self.store.enqueue(Stage::Embed, "corpus", &src.id).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Embedding)
                    .await?;
            }
            // Both look at the whole collection; one corpus cannot be reprocessed through them.
            Stage::Consolidate | Stage::Associate => {
                return Err(Error::Validation(
                    "that stage is a collection-wide sweep, not a per-corpus stage".into(),
                ));
            }
            // Units the queue arms for itself; an operator reprocesses a document, not a window.
            Stage::SegmentWindow
            | Stage::Title
            | Stage::Dedupe
            | Stage::Relate
            | Stage::Probe
            | Stage::Condense
            | Stage::LinkJudge
            | Stage::Generate => {
                return Err(Error::Validation(
                    "that stage is a single inference call the queue arms itself; \
                     reprocess the document instead"
                        .into(),
                ));
            }
            // These sweeps look at every recorded search or the whole collection, not one corpus.
            Stage::Pursuit
            | Stage::Retention
            | Stage::ArmDedupe
            | Stage::Context
            | Stage::Remind
            | Stage::Reap => {
                return Err(Error::Validation(
                    "that stage is a collection-wide sweep, not a per-corpus stage".into(),
                ));
            }
            Stage::Fetch => {
                if src.origin != ORIGIN_FETCH || src.source_url.is_none() {
                    return Err(Error::Validation(
                        "only a captured link can be fetched again".into(),
                    ));
                }
                let mut meta = src.metadata.clone();
                let title_given = meta["fetch"]["title_given"].as_bool().unwrap_or(false);
                meta["fetch"] = serde_json::json!({
                    "pending": true,
                    "title_given": title_given,
                });
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.store.enqueue(Stage::Fetch, "corpus", &src.id).await?;
            }
            // Replaced wholesale: an artifact of the old reading has no span in the new one.
            Stage::Extract => {
                if src.origin != ORIGIN_PDF || !self.store.has_attachment(&src.id).await? {
                    return Err(Error::Validation(
                        "only a captured PDF can be re-extracted".into(),
                    ));
                }
                self.forget_derived_work(&src.id).await?;
                self.store.clear_read_text(&src.id).await?;
                let mut meta = src.metadata.clone();
                if let Some(m) = meta.as_object_mut() {
                    m.remove("extract");
                }
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Extracting)
                    .await?;
                self.heal_dangling_supersessions().await?;
                self.store
                    .enqueue(Stage::Extract, "corpus", &src.id)
                    .await?;
            }
            // Replaced wholesale: a chunk of the old reading has no span in the new one.
            Stage::Describe => {
                // Origin, not attachment: a PDF has one too, and has no preview.
                if src.origin != ORIGIN_IMAGE || !self.store.has_attachment(&src.id).await? {
                    return Err(Error::Validation(
                        "only a captured image can be re-read".into(),
                    ));
                }
                if self.describer.is_none() {
                    return Err(Error::Validation(
                        "image capture is not configured — set [infer.vision] to enable it".into(),
                    ));
                }
                self.forget_derived_work(&src.id).await?;
                self.store.clear_read_text(&src.id).await?;
                let mut meta = src.metadata.clone();
                if let Some(m) = meta.as_object_mut() {
                    m.remove("describe");
                }
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Describing)
                    .await?;
                self.heal_dangling_supersessions().await?;
                self.store
                    .enqueue(Stage::Describe, "corpus", &src.id)
                    .await?;
            }
        }
        Ok(())
    }
}

/// Whether unread lines are a loss rather than windows still pending; `partial` and `failed` count.
pub(crate) fn coverage_final(status: &CorpusStatus) -> bool {
    matches!(
        status,
        CorpusStatus::Ready | CorpusStatus::Partial | CorpusStatus::Failed
    )
}

impl Core {
    /// Re-read the window holding these lines, one model call; nothing written is replaced.
    /// False wherever there is nothing to re-read, so the phone can say so.
    pub async fn reread_uncovered(&self, cid: &str, from: i64, to: i64) -> Result<bool> {
        // A re-read is a model call, and the embed-only floor has no model.
        if !self.synthesizes() {
            return Ok(false);
        }
        // A capture still being read has lines merely not written yet.
        let s = self.store.get_corpus(cid).await?;
        if !coverage_final(&s.status) {
            return Ok(false);
        }
        // The same cut the page renders; placeholders and span-less artifacts render nothing red.
        let chunks = self.store.artifacts_for_corpus(cid).await?;
        if s.restored_at.is_some() || chunks.iter().any(|c| c.corpus_span.is_none()) {
            return Ok(false);
        }
        let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = chunks
            .iter()
            .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
            .collect();
        let lost: Vec<(i64, i64)> = crate::core::coverage::bands(&s.raw_text, &spans, None)
            .into_iter()
            .filter(|b| b.gap() && b.from <= to && from <= b.to)
            .map(|b| (b.from, b.to))
            .collect();
        if lost.is_empty() {
            return Ok(false);
        }

        let segments = self.store.segments_for_corpus(cid).await?;
        let mut queued = false;
        for w in segments.iter().filter(|w| {
            lost.iter()
                .any(|(a, z)| w.start_line <= *z && *a <= w.end_line)
        }) {
            if self
                .store
                .live_job(
                    crate::store::jobs::Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(cid, w.idx),
                )
                .await?
            {
                continue;
            }
            self.store.reset_segment(cid, w.idx, true).await?;
            self.store
                .enqueue(
                    crate::store::jobs::Stage::SegmentWindow,
                    "segment",
                    &crate::jobs::window::unit_target(cid, w.idx),
                )
                .await?;
            queued = true;
        }
        Ok(queued)
    }
}

/// One definition, read by `heal_store_drift` and by `--restore` / `--verify-stores`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Drift {
    /// Points no row names: unreachable content, only ever reported or pruned.
    pub orphan_points: Vec<String>,
    /// Rows SQLite calls embedded with no point: re-armed for embedding.
    pub missing_points: Vec<String>,
}

pub(crate) fn store_drift(rows: &[String], embedded: &[String], points: &[String]) -> Drift {
    use std::collections::HashSet;
    let has_row: HashSet<&str> = rows.iter().map(String::as_str).collect();
    let has_point: HashSet<&str> = points.iter().map(String::as_str).collect();
    Drift {
        orphan_points: points
            .iter()
            .filter(|id| !has_row.contains(id.as_str()))
            .cloned()
            .collect(),
        missing_points: embedded
            .iter()
            .filter(|id| !has_point.contains(id.as_str()))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use crate::core::ingest::MAX_TEXT_BYTES;
    use crate::core::ingest::{
        Capture, ImageCapture, MAX_NOTE_CHARS, ORIGIN_FETCH, ORIGIN_IMAGE, ORIGIN_PDF, PdfCapture,
    };
    use crate::core::ingest::{Drift, store_drift};
    use crate::core::test_support::test_core;
    use crate::error::Error;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn store_drift_of_agreeing_stores_is_empty() {
        let d = store_drift(&ids(&["a", "b"]), &ids(&["a"]), &ids(&["a"]));
        assert_eq!(d, Drift::default());
    }

    #[test]
    fn store_drift_names_a_point_without_a_row() {
        let d = store_drift(&ids(&["a"]), &ids(&["a"]), &ids(&["a", "gone"]));
        assert_eq!(d.orphan_points, ids(&["gone"]));
        assert!(d.missing_points.is_empty());
    }

    #[test]
    fn store_drift_names_an_embedded_row_without_a_point() {
        // `b` is a row that is not embedded: no point is owed, so it is not missing.
        let d = store_drift(&ids(&["a", "b", "c"]), &ids(&["a", "c"]), &ids(&["a"]));
        assert_eq!(d.missing_points, ids(&["c"]));
        assert!(d.orphan_points.is_empty());
    }

    #[tokio::test]
    async fn a_file_over_its_kind_s_ceiling_is_refused() {
        let mut core = test_core().await;
        core.capture.image_max_bytes = 1024 * 1024;
        core.capture.pdf_max_bytes = 2 * 1024 * 1024;

        // Never a decodable image: refused on weight alone, before the hash and the decode permit.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(2 * 1024 * 1024, 0);
        let err = core
            .ingest_file(
                png,
                Some("photo.png".into()),
                None,
                None,
                "share",
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect_err("an oversize image is refused");
        assert!(err.to_string().contains("1 MB limit"), "{err}");

        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(3 * 1024 * 1024, 0);
        let err = core
            .ingest_file(
                pdf,
                Some("book.pdf".into()),
                None,
                None,
                "share",
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect_err("an oversize PDF is refused");
        assert!(err.to_string().contains("2 MB limit"), "{err}");

        let text = vec![b'a'; MAX_TEXT_BYTES + 1];
        let err = core
            .ingest_file(
                text,
                Some("notes.txt".into()),
                None,
                None,
                "share",
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect_err("an oversize text file is refused");
        assert!(err.to_string().contains("8 MB limit"), "{err}");
    }

    #[tokio::test]
    async fn a_url_holding_a_page_is_captured_with_its_provenance() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let body = format!(
            "<html><body><article><h1>Mounting</h1>{}</article></body></html>",
            "<p>Run mount, then check dmesg for the device name and the filesystem it found.</p>"
                .repeat(6)
        );
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
            .mount(&server)
            .await;
        let core = test_core().await;
        let u = url::Url::parse(&format!("{}/page", server.uri())).unwrap();
        let out = core
            .ingest_url(
                &u,
                None,
                Some("from the agent".into()),
                crate::infer::lang::Lang::default(),
            )
            .await
            .unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, ORIGIN_FETCH);
        assert_eq!(c.source_url.as_deref(), Some(u.as_str()));
        assert!(c.raw_text.contains("check dmesg"), "{}", c.raw_text);
        assert_eq!(c.metadata["note"], "from the agent");
    }

    #[tokio::test]
    async fn a_url_holding_a_pdf_is_stored_for_extraction_under_its_name() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/papers/plan.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(a_pdf_fixture(), "application/pdf"),
            )
            .mount(&server)
            .await;
        let core = test_core().await;
        let u = url::Url::parse(&format!("{}/papers/plan.pdf", server.uri())).unwrap();
        let out = core
            .ingest_url(&u, None, None, crate::infer::lang::Lang::default())
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Extracting);
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, ORIGIN_PDF);
        assert_eq!(c.metadata["file"]["name"], "plan.pdf");
        assert_eq!(c.source_url.as_deref(), Some(u.as_str()));
    }

    fn a_pdf_fixture() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    fn a_pdf_capture() -> PdfCapture {
        PdfCapture {
            bytes: a_pdf_fixture(),
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: Some("the quarterly plan".into()),
            lang: crate::infer::lang::Lang::default(),
        }
    }

    #[tokio::test]
    async fn a_pdf_is_stored_whole_and_queued_to_be_extracted() {
        let core = test_core().await;
        let out = core.ingest_pdf(a_pdf_capture()).await.unwrap();

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Extracting);
        assert_eq!(src.origin, ORIGIN_PDF);
        assert_eq!(
            src.raw_text, "",
            "the text arrives from the stage, not here"
        );
        assert_eq!(src.metadata["note"], "the quarterly plan");
        assert_eq!(src.metadata["file"]["name"], "plan.pdf");
        assert_eq!(src.metadata["file"]["mime"], "application/pdf");

        let (mime, bytes) = core
            .store
            .attachment_original(&out.id)
            .await
            .unwrap()
            .expect("the PDF itself is kept");
        assert_eq!(mime, "application/pdf");
        assert_eq!(bytes, a_pdf_fixture(), "stored byte for byte");

        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Extract);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn the_same_pdf_twice_is_one_corpus() {
        let core = test_core().await;
        let first = core.ingest_pdf(a_pdf_capture()).await.unwrap();
        let again = core.ingest_pdf(a_pdf_capture()).await.unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
    }

    #[tokio::test]
    async fn the_pdf_door_is_open_without_any_model_configured() {
        let core = crate::core::test_support::test_core_without_vision().await;
        assert!(core.ingest_pdf(a_pdf_capture()).await.is_ok());
    }

    #[tokio::test]
    async fn a_file_is_read_as_what_its_bytes_say_it_is_under_the_origin_given() {
        let core = test_core().await;

        let text = core
            .ingest_file(
                b"a procedure worth keeping".to_vec(),
                Some("notes.txt".into()),
                None,
                None,
                "cli",
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect("text file");
        let stored = core.store.get_corpus(&text.id).await.expect("stored");
        assert_eq!(
            stored.origin, "cli",
            "the caller's origin is what is recorded"
        );

        let png = core
            .ingest_file(
                a_seeded_png(9),
                Some("shot.png".into()),
                None,
                None,
                "share",
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect("image file");
        assert_eq!(
            png.status,
            CorpusStatus::Describing,
            "an image is read by a job"
        );

        let refused = core
            .ingest_file(
                vec![0xff, 0xfe, 0x00],
                None,
                None,
                None,
                "cli",
                crate::infer::lang::Lang::default(),
            )
            .await;
        assert!(
            refused.is_err(),
            "bytes that are no format we read are refused"
        );
    }

    fn a_seeded_png(seed: u8) -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(16, 16, |x, y| Rgb([seed, x as u8, y as u8]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn an_image(seed: u8) -> ImageCapture {
        ImageCapture {
            bytes: a_seeded_png(seed),
            filename: None,
            title_hint: None,
            note: None,
            lang: crate::infer::lang::Lang::default(),
        }
    }

    #[tokio::test]
    async fn only_so_many_uploads_are_decoded_at_once() {
        let core = test_core().await;
        // Stand in for the decodes already running: hold every permit, and the
        // next capture must wait rather than start a decode of its own.
        let held = core
            .decodes
            .clone()
            .acquire_many_owned(crate::core::image::MAX_CONCURRENT_DECODES as u32)
            .await
            .unwrap();

        let mut waiting = tokio::spawn({
            let core = core.clone();
            async move { core.ingest_image(an_image(11)).await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut waiting)
                .await
                .is_err(),
            "a capture must not decode while the permits are all taken"
        );
        waiting.abort();

        drop(held);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            core.ingest_image(an_image(12)),
        )
        .await
        .expect("released permits let the next capture through")
        .unwrap();
        assert!(!out.duplicate);
    }

    #[tokio::test]
    async fn re_reading_a_failed_image_clears_the_reason_and_queues_describe_again() {
        let core = test_core().await;
        let id = core.ingest_image(an_image(1)).await.unwrap().id;
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400")
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        core.store.complete_job(job.id).await.unwrap();

        core.reprocess(&id, Stage::Describe).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert!(src.metadata.get("describe").is_none());
        let job = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("describe re-armed");
        assert_eq!(
            (job.stage, job.target_id.as_str()),
            (Stage::Describe, id.as_str())
        );
    }

    #[tokio::test]
    async fn re_reading_a_ready_image_starts_it_over_from_the_pixels() {
        let core = test_core().await;
        let id = core.ingest_image(an_image(2)).await.unwrap().id;
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Ready
        );

        core.reprocess(&id, Stage::Describe).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert_eq!(src.raw_text, "");
        assert!(src.shingles.is_empty());
        assert!(
            core.store
                .artifacts_for_corpus(&id)
                .await
                .unwrap()
                .is_empty()
        );
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn re_segmenting_an_image_that_has_not_been_read_is_refused() {
        let core = test_core().await;
        let id = core.ingest_image(an_image(3)).await.unwrap().id;
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400")
            .await
            .unwrap();
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
        assert!(core.store.live_job(Stage::Describe, &id).await.unwrap());
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    #[tokio::test]
    async fn a_duplicate_image_is_recognised_by_the_shared_hash() {
        let core = test_core().await;
        let bytes = a_seeded_png(4);
        let first = core
            .ingest_image(ImageCapture {
                bytes: bytes.clone(),
                filename: None,
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        let src = core.store.get_corpus(&first.id).await.unwrap();
        assert_eq!(
            src.content_hash,
            crate::store::corpora::content_hash(&bytes)
        );
        let again = core
            .ingest_image(ImageCapture {
                bytes,
                filename: None,
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
    }

    #[tokio::test]
    async fn an_image_filename_is_kept_as_a_file_fact_and_not_used_as_its_title() {
        let core = test_core().await;
        let id = core
            .ingest_image(ImageCapture {
                bytes: a_seeded_png(5),
                filename: Some("photo.jpg".into()),
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap()
            .id;
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.title_hint, None);
        assert_eq!(src.metadata["file"]["name"], "photo.jpg");
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert!(
            core.store
                .get_corpus(&id)
                .await
                .unwrap()
                .title_hint
                .is_some(),
            "the Title stage named it"
        );
    }

    #[tokio::test]
    async fn a_text_corpus_cannot_be_re_read() {
        let core = test_core().await;
        let src = core.ingest("some text", "web", None).await.unwrap();
        assert!(matches!(
            core.reprocess(&src.id, Stage::Describe).await,
            Err(Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn a_pdf_cannot_be_re_read_through_the_vision_stage() {
        let core = test_core().await;
        let out = core.ingest_pdf(a_pdf_capture()).await.unwrap();
        core.store
            .set_read_text(&out.id, "# Plan\n\nthe text docling found", vec![])
            .await
            .unwrap();

        assert!(matches!(
            core.reprocess(&out.id, Stage::Describe).await,
            Err(Error::Validation(_))
        ));

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(
            src.raw_text, "# Plan\n\nthe text docling found",
            "the extraction survived the refusal"
        );
        assert_ne!(src.status, CorpusStatus::Describing);
    }

    #[tokio::test]
    async fn healing_a_dangling_supersession_leaves_no_unfinished_write_behind() {
        // `repair_lifecycle_drift` returns a count so a repair on an agreeing base is visible.
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])],
        )
        .await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "supersede itself left the write marked as unfinished"
        );

        // Through `Core`, not the store: the heal hangs off this deletion.
        core.delete_artifact(&ids[1]).await.unwrap();

        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the artifact still points at a keeper that is gone"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the healed artifact is still marked as an unfinished write"
        );
    }

    async fn stamped_at(core: &crate::core::Core, id: &str) -> Option<i64> {
        core.store.get_artifact(id).await.unwrap().last_verified_at
    }

    fn an_ask_citing(ids: &[(&str, bool)]) -> crate::store::asks::NewAsk {
        crate::store::asks::NewAsk {
            question: "how do I mount it".into(),
            scope: Some("me".into()),
            filters: "{}".into(),
            query_vec: vec![0.1, 0.2],
            embed_model: "fake".into(),
            answer: "an answer".into(),
            citations: ids
                .iter()
                .map(|(id, used)| crate::store::asks::NewAskCitation {
                    artifact_id: (*id).to_string(),
                    score: 1.0,
                    used: *used,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_right_answer_confirms_what_it_cited_and_a_wrong_one_does_not() {
        use crate::store::asks::AskVerdict;
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("used", [1.0, 0.0]), ("shown only", [0.0, 1.0])],
        )
        .await;
        for id in &ids {
            core.store.set_last_verified_at(id, 1).await.unwrap();
        }

        let right = core
            .store
            .record_ask(an_ask_citing(&[(&ids[0], true), (&ids[1], false)]))
            .await
            .unwrap();
        core.judge_ask(&right, AskVerdict::Right).await.unwrap();
        assert!(
            stamped_at(&core, &ids[0]).await.unwrap() > 1,
            "the note the answer used was not confirmed"
        );
        assert_eq!(
            stamped_at(&core, &ids[1]).await,
            Some(1),
            "a note that was only shown was confirmed"
        );

        core.store.set_last_verified_at(&ids[0], 1).await.unwrap();
        let wrong = core
            .store
            .record_ask(an_ask_citing(&[(&ids[0], true)]))
            .await
            .unwrap();
        core.judge_ask(&wrong, AskVerdict::Wrong).await.unwrap();
        assert_eq!(
            stamped_at(&core, &ids[0]).await,
            Some(1),
            "a wrong answer confirmed its source"
        );
        let ev = core.store.ask_event(&wrong).await.unwrap().unwrap();
        assert!(matches!(ev.verdict, Some(AskVerdict::Wrong)));
    }

    #[tokio::test]
    async fn confirming_an_artifact_out_of_results_is_skipped_not_an_error() {
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(&core, &[("retired", [1.0, 0.0])]).await;
        core.store.set_last_verified_at(&ids[0], 1).await.unwrap();
        core.deprecate(&ids[0]).await.unwrap();

        core.confirm_cited(&[ids[0].clone(), "no-such-artifact".to_string()])
            .await;

        assert_eq!(stamped_at(&core, &ids[0]).await, Some(1));
    }

    #[tokio::test]
    async fn a_capture_remembers_where_it_came_from() {
        let core = test_core().await;
        let out = core
            .ingest_capture(
                crate::core::ingest::Capture::new("alpha para\n\nbeta para", "extension")
                    .with_source_url(Some("https://example.test/notes".into())),
            )
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.origin, "extension");
        assert_eq!(
            src.source_url.as_deref(),
            Some("https://example.test/notes")
        );
    }

    #[tokio::test]
    async fn an_ordinary_capture_has_no_source_url() {
        let core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.source_url, None);
    }

    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn reprocessing_forgets_the_previous_runs_coverage() {
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        core.store
            .set_corpus_coverage(&out.id, Some(0.87))
            .await
            .unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();

        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .coverage
                .is_none(),
            "the rerun carried the previous run's coverage, hiding it from the repair sweep"
        );
    }

    #[tokio::test]
    async fn reprocessing_gives_every_window_its_attempts_back() {
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query!("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar!("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.control.pool)
                .await
                .unwrap();
        assert!(
            !attempts.is_empty(),
            "the rerun should have armed its windows again"
        );
        assert!(
            attempts.iter().all(|&a| a == 0),
            "the rerun inherited the previous run's attempts: {attempts:?}"
        );
    }

    #[tokio::test]
    async fn reprocessing_gives_a_corpus_that_was_never_named_another_chance() {
        let core = test_core().await;
        let src = core.ingest(&manual("mount"), "web", None).await.unwrap();
        for _ in 0..200 {
            sqlx::query!("UPDATE jobs SET run_after = 0 WHERE state = 'pending'")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(&core).await.unwrap_or(false) {
                break;
            }
        }
        // Capture names locally, so put the corpus in the state the title unit exists for.
        sqlx::query!("UPDATE corpora SET title_hint = NULL WHERE id = ?", &src.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        crate::jobs::synthesize::finish(&core, &src.id)
            .await
            .unwrap();
        assert!(
            core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "the fixture must arm a title unit"
        );

        core.reprocess(&src.id, Stage::Synthesize).await.unwrap();

        assert!(
            !core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "reprocess left the spent title unit behind, so the corpus can never be named"
        );
    }

    #[tokio::test]
    async fn a_near_identical_capture_is_flagged_and_still_read_and_embedded() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let edited = manual("mount").replacen("step 7:", "step seven:", 1);
        let second = core.ingest(&edited, "web", None).await.unwrap();

        assert_ne!(second.id, first.id, "the capture must still be stored");
        assert!(!second.duplicate, "it is not a byte-identical duplicate");
        assert_eq!(second.status, CorpusStatus::Raw);
        let near = second.near_duplicate.expect("no near-duplicate reported");
        assert_eq!(near.corpus_id, first.id);
        assert!(near.similarity > 0.90);
        let row = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(row.near_dupe_of.as_deref(), Some(first.id.as_str()));

        crate::jobs::test_support::drain(&core).await;
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::Ready,
            "a flagged capture is read and embedded like any other"
        );
        assert!(
            core.store
                .get_corpus(&second.id)
                .await
                .unwrap()
                .near_dupe_of
                .is_some(),
            "and stays flagged, which is a note on it rather than a question"
        );
    }

    #[tokio::test]
    async fn a_capture_an_older_build_parked_is_released_by_the_sweep() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let src = core
            .store
            .insert_corpus_with_signature(
                &manual("pastry"),
                "web",
                None,
                vec![],
                None,
                &serde_json::json!({}),
                crate::store::corpora::Followup::Nothing,
            )
            .await
            .unwrap();
        let crate::store::corpora::Insertion::Created(src) = src else {
            panic!("fixture corpus already existed")
        };
        core.store
            .set_corpus_status(&src.id, CorpusStatus::NeedsReview)
            .await
            .unwrap();
        core.store
            .set_near_dupe(&src.id, Some(&first.id), Some(0.9))
            .await
            .unwrap();

        crate::jobs::reconcile::run(&core).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let got = core.store.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Ready);
        assert_eq!(got.near_dupe_of.as_deref(), Some(first.id.as_str()));
    }

    #[tokio::test]
    async fn an_ordinary_capture_is_unaffected() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let out = core.ingest(&manual("pastry"), "web", None).await.unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
        assert!(out.near_duplicate.is_none());
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "an unrelated capture must still queue synthesis"
        );
    }

    #[tokio::test]
    async fn reprocessing_a_flagged_capture_clears_its_flag() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        assert!(
            core.store
                .get_corpus(&second.id)
                .await
                .unwrap()
                .near_dupe_of
                .is_some()
        );

        core.reprocess(&second.id, Stage::Synthesize).await.unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert!(
            got.near_dupe_of.is_none(),
            "the capture was reprocessed and is still flagged"
        );
        assert_eq!(got.status, CorpusStatus::Raw);
    }

    #[tokio::test]
    async fn ingest_returns_immediately_and_enqueues_segmentation() {
        let core = test_core().await;
        let out = core
            .ingest("a procedure", "web", Some("title"))
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
        assert!(!out.duplicate);

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.raw_text, "a procedure");
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "segment job was not enqueued"
        );
    }

    #[tokio::test]
    async fn ingest_is_not_blocked_by_a_dead_chunker() {
        let mut core = test_core().await;
        core.synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::failing("endpoint down"),
        ));
        let out = core.ingest("still accepted", "web", None).await.unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
    }

    #[tokio::test]
    async fn identical_text_returns_the_existing_source() {
        let core = test_core().await;
        let a = core.ingest("same words", "web", None).await.unwrap();
        let b = core.ingest("same words", "mcp", None).await.unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.duplicate);
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_captures_of_the_same_text_at_once_both_get_an_answer() {
        let core = test_core().await;
        let (a, b) = tokio::join!(
            core.ingest("filed twice at once", "web", None),
            core.ingest("filed twice at once", "mcp", None),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.id, b.id);
        assert!(
            a.duplicate ^ b.duplicate,
            "exactly one of the two should be the duplicate: {a:?} {b:?}"
        );
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.ingest("   \n ", "web", None).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn deleting_a_source_also_deletes_its_vectors() {
        let core = test_core().await;
        let out = core.ingest("some text", "web", None).await.unwrap();
        let chunks = core
            .store
            .insert_artifacts(
                &out.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "t".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                sparse: Default::default(),
                vector: vec![0.1; 8],
                payload: crate::vector::VectorPayload {
                    artifact_id: chunks[0].id.clone(),
                    corpus_id: out.id.clone(),
                    text: "t".into(),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();

        core.delete_corpus(&out.id).await.unwrap();
        assert_eq!(
            core.vectors.count().await.unwrap(),
            0,
            "orphaned vectors would still be searchable"
        );
        assert!(matches!(
            core.store.get_corpus(&out.id).await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn deleting_a_missing_source_is_not_found() {
        let core = test_core().await;
        assert!(matches!(
            core.delete_corpus("nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn reprocess_requeues_the_requested_stage() {
        let core = test_core().await;
        let out = core.ingest("text", "web", None).await.unwrap();
        core.store.claim_job().await.unwrap(); // drain the initial segment job
        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        let j = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(j.target_id, out.id);
        assert_eq!(
            j.attempts, 1,
            "reprocess must start from a clean attempt count"
        );
    }

    #[tokio::test]
    async fn reprocessing_re_segments_instead_of_finding_every_window_done() {
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let first = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        assert!(first > 0);

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        assert!(
            core.store
                .segments_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "reprocess must forget the windowing so it can be redone"
        );

        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            first,
            "the rerun must produce the chunks again, not an empty source"
        );
        assert_ne!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    fn point(artifact_id: &str, corpus_id: &str) -> crate::vector::VectorPoint {
        crate::vector::VectorPoint {
            sparse: Default::default(),
            vector: vec![0.1; 8],
            payload: crate::vector::VectorPayload {
                artifact_id: artifact_id.to_string(),
                corpus_id: corpus_id.to_string(),
                text: "t".into(),
                ..Default::default()
            },
        }
    }

    async fn one_artifact(core: &crate::core::Core) -> (String, String) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "t".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        (src.id, made[0].id.clone())
    }

    #[tokio::test]
    async fn deprecating_an_already_superseded_artifact_is_refused() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "loser".into(),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "winner".into(),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        core.supersede(&made[0].id, &made[1].id).await.unwrap();

        assert!(matches!(
            core.deprecate(&made[0].id).await,
            Err(crate::error::Error::Validation(_))
        ));
        let after = core.store.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(
            after.status,
            crate::store::artifacts::ArtifactStatus::Superseded,
            "the refused call changed the row anyway"
        );
    }

    #[tokio::test]
    async fn a_row_that_claims_it_is_embedded_with_no_point_is_requeued() {
        let core = test_core().await;
        let (_, artifact) = one_artifact(&core).await;
        core.store
            .mark_embedded(&artifact, "some-model", 0)
            .await
            .unwrap();

        core.heal_store_drift().await.unwrap();

        assert!(
            core.store.live_job(Stage::Embed, &artifact).await.unwrap(),
            "the row was not sent back through the embed pipeline"
        );
        assert!(
            core.store.get_artifact(&artifact).await.is_ok(),
            "the heal deleted the row instead of re-embedding it"
        );
    }

    #[tokio::test]
    async fn the_drift_repair_leaves_a_retrying_embed_units_backoff_alone() {
        // A failing unit is `pending` with `run_after` in the future; re-arming would reset its
        // backoff.
        let core = test_core().await;
        let (_, artifact) = one_artifact(&core).await;
        core.store
            .mark_embedded(&artifact, "some-model", 0)
            .await
            .unwrap();
        core.heal_store_drift().await.unwrap();

        let job = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(job.stage, Stage::Embed);
        core.store
            .fail_job(job.id, job.attempts, "endpoint down")
            .await
            .unwrap();
        let before: (i64, i64) =
            sqlx::query!("SELECT attempts, run_after FROM jobs WHERE id = ?", job.id)
                .fetch_one(&core.store.control.pool)
                .await
                .map(|r| (r.attempts, r.run_after))
                .unwrap();
        assert!(before.0 > 0 && before.1 > 0, "the unit is not backing off");

        core.heal_store_drift().await.unwrap();

        let after: (i64, i64) =
            sqlx::query!("SELECT attempts, run_after FROM jobs WHERE id = ?", job.id)
                .fetch_one(&core.store.control.pool)
                .await
                .map(|r| (r.attempts, r.run_after))
                .unwrap();
        assert_eq!(
            after, before,
            "the drift repair cleared a retrying unit's backoff"
        );
    }

    #[tokio::test]
    async fn a_point_whose_row_is_gone_is_reported_and_left_alone() {
        let core = test_core().await;
        let (corpus, artifact) = one_artifact(&core).await;
        core.vectors
            .upsert(vec![point(&artifact, &corpus), point("gone", &corpus)])
            .await
            .unwrap();

        core.heal_store_drift().await.unwrap();

        assert_eq!(core.vectors.all_artifact_ids().await.unwrap().len(), 2);
        assert!(
            core.store.get_artifact("gone").await.is_err(),
            "a row was invented"
        );
    }

    #[tokio::test]
    async fn an_artifact_still_waiting_to_embed_is_not_drift() {
        let core = test_core().await;
        let (_, artifact) = one_artifact(&core).await;
        core.heal_store_drift().await.unwrap();

        assert!(
            !core.store.live_job(Stage::Embed, &artifact).await.unwrap(),
            "the backlog was re-queued"
        );
    }

    #[tokio::test]
    async fn an_image_capture_stores_the_original_and_queues_describe_without_calling_the_model() {
        let describer = std::sync::Arc::new(crate::infer::fake::FakeDescriber::default());
        let core = crate::core::test_support::test_core_with_describer(describer.clone()).await;
        let bytes = a_seeded_png(7);
        let out = core
            .ingest_image(ImageCapture {
                bytes: bytes.clone(),
                filename: Some("IMG_1.png".into()),
                title_hint: None,
                note: Some("  the kitchen whiteboard ".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Describing);
        assert!(!out.duplicate);
        assert_eq!(describer.calls(), 0, "capture must not call the model");

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.origin, ORIGIN_IMAGE);
        assert_eq!(src.raw_text, "");
        assert_eq!(
            src.title_hint, None,
            "a filename is a file fact; the Title stage names the capture"
        );
        assert_eq!(src.metadata["note"], "the kitchen whiteboard");
        assert_eq!(src.metadata["file"]["name"], "IMG_1.png");
        assert_eq!(src.metadata["file"]["mime"], "image/png");
        assert_eq!(src.metadata["file"]["width"], 16);

        let a = core
            .store
            .attachment_for_corpus(&out.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.bytes, bytes, "the original is stored byte for byte");
        assert_eq!(a.mime, "image/png");
        assert!(image::load_from_memory(&a.preview).is_ok());

        let job = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("a job was queued");
        assert_eq!(job.stage, Stage::Describe);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn without_a_vision_role_the_image_is_stored_to_be_read_later() {
        let core = crate::core::test_support::test_core_without_vision().await;
        let out = core
            .ingest_image(ImageCapture {
                bytes: a_seeded_png(7),
                filename: None,
                title_hint: None,
                note: Some("the whiteboard after the planning meeting".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .expect("a photo is stored without a vision role");
        assert_eq!(out.status, CorpusStatus::Describing);
        assert!(core.store.has_job(Stage::Describe, &out.id).await.unwrap());
        assert_eq!(
            core.store
                .pending_artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            1,
            "the note is waiting for its vector, so it is findable"
        );
    }

    #[tokio::test]
    async fn junk_is_refused_before_anything_is_stored() {
        let core = crate::core::test_support::test_core().await;
        let e = core
            .ingest_image(ImageCapture {
                bytes: b"nope".to_vec(),
                filename: Some("x.jpg".into()),
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_long_note_is_stored_whole_and_a_blank_one_is_dropped() {
        let core = test_core().await;
        let long = "x".repeat(MAX_NOTE_CHARS + 50);
        let out = core
            .ingest_capture(Capture::new("some text", "upload").with_note(Some(long.clone())))
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.metadata["note"].as_str().unwrap(), long);
        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(all[0].text, long, "the artifact keeps every character");

        let out = core
            .ingest_capture(Capture::new("other text", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .metadata
                .get("note")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_text_capture_carries_its_file_facts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(Capture::new("hello", "upload").with_file(
                Some("n.txt"),
                5,
                "text/plain",
            ))
            .await
            .unwrap();
        let m = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(
            m["file"],
            serde_json::json!({"name": "n.txt", "size": 5, "mime": "text/plain"})
        );
    }

    #[tokio::test]
    async fn undoing_a_promotion_restores_the_passages_deprecates_the_artifacts_and_resets_the_window()
     {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "l1\nl2",
                }],
            )
            .await
            .unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: 1,
                end_line: 2,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            segment_idx: Some(0),
            ..Default::default()
        };
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(0, "passage")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let a = core
            .store
            .insert_artifacts(&src.id, &[na(1, "artifact")])
            .await
            .unwrap();
        core.supersede(&p[0].id, &a[0].id).await.unwrap();
        core.store
            .set_segment_state(&src.id, 0, crate::store::segments::SegmentState::Done, None)
            .await
            .unwrap();

        core.undo_promotion(&src.id, 0).await.unwrap();

        assert!(
            core.store
                .get_artifact(&p[0].id)
                .await
                .unwrap()
                .in_results()
        );
        assert_eq!(
            core.store.get_artifact(&a[0].id).await.unwrap().status,
            crate::store::artifacts::ArtifactStatus::Deprecated
        );
        assert_eq!(
            core.store.segment_state(&src.id, 0).await.unwrap(),
            Some(crate::store::segments::SegmentState::Verbatim)
        );
    }

    #[tokio::test]
    async fn a_note_on_a_pdf_becomes_a_span_less_artifact_on_its_corpus() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("lease.pdf".into()),
                title_hint: None,
                note: Some("  scan of the Reinhardt lease, break clause is p.3  ".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();

        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(all.len(), 1, "the note, and nothing extracted yet");
        let n = &all[0];
        assert_eq!(n.text, "scan of the Reinhardt lease, break clause is p.3");
        assert_eq!(
            n.corpus_span, None,
            "the note is about the file, not a line of it"
        );
        assert_eq!(n.segment_idx, None, "it belongs to no window");
        assert_eq!(n.ordinal, 0);
        assert_eq!(n.provenance, crate::store::artifacts::Provenance::Note);
        assert_eq!(n.title, None);
    }

    #[tokio::test]
    async fn every_door_that_takes_a_note_writes_it_as_an_artifact() {
        let describer = std::sync::Arc::new(crate::infer::fake::FakeDescriber::default());
        let core = crate::core::test_support::test_core_with_describer(describer).await;

        let img = core
            .ingest_image(ImageCapture {
                bytes: a_seeded_png(11),
                filename: Some("IMG_9.png".into()),
                title_hint: None,
                note: Some("front of the router".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        let a = core.store.artifacts_for_corpus(&img.id).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "front of the router");

        let txt = core
            .ingest_capture(
                Capture::new("the file's own text", "upload")
                    .with_note(Some("from the printer".into())),
            )
            .await
            .unwrap();
        let a = core.store.artifacts_for_corpus(&txt.id).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "from the printer");
    }

    #[tokio::test]
    async fn a_capture_with_no_usable_note_writes_no_artifact() {
        let core = test_core().await;
        let none = core
            .ingest_capture(Capture::new("text one", "upload"))
            .await
            .unwrap();
        assert!(
            core.store
                .artifacts_for_corpus(&none.id)
                .await
                .unwrap()
                .is_empty()
        );

        let blank = core
            .ingest_capture(Capture::new("text two", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(
            core.store
                .artifacts_for_corpus(&blank.id)
                .await
                .unwrap()
                .is_empty(),
            "whitespace is not an annotation"
        );
    }

    #[tokio::test]
    async fn a_note_arms_the_embed_so_an_unread_capture_still_becomes_findable() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("scan.pdf".into()),
                title_hint: None,
                note: Some("the survey nobody can OCR".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();

        let pending = core
            .store
            .pending_artifacts_for_corpus(&out.id)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "the note is waiting for a vector");
        assert!(
            core.store.live_job(Stage::Embed, &out.id).await.unwrap(),
            "a corpus embed job must be armed by the capture itself"
        );
    }

    #[tokio::test]
    async fn a_duplicate_upload_writes_no_second_note() {
        let core = test_core().await;
        let bytes = a_pdf_fixture();
        let first = core
            .ingest_pdf(PdfCapture {
                bytes: bytes.clone(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("the quarterly plan".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        let again = core
            .ingest_pdf(PdfCapture {
                bytes,
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("a second thought about it".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        assert_eq!(first.id, again.id);
        assert!(again.duplicate);
        assert_eq!(
            core.store
                .artifacts_for_corpus(&first.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_reprocess_keeps_the_note_and_queues_it_for_a_fresh_vector() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("lease.pdf".into()),
                title_hint: None,
                note: Some("break clause is p.3".into()),
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap();
        let before = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        let note = &before[0];
        assert!(
            core.store
                .mark_embedded(&note.id, "bge-m3", note.embed_rev)
                .await
                .unwrap()
        );

        core.reprocess(&out.id, Stage::Extract).await.unwrap();

        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "the reprocess deleted the operator's own note"
        );
        assert_eq!(all[0].text, "break clause is p.3");
        assert_eq!(
            core.store
                .pending_artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            1,
            "its vector went with the corpus; the note has to be embedded again"
        );
    }

    #[tokio::test]
    async fn a_forced_remind_is_a_reminder_and_keeps_its_origin() {
        let core = test_core().await;
        let out = core
            .ingest_capture(
                Capture::new("Heute den Bericht abgeben.", "ui")
                    .with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, "ui");
        crate::jobs::test_support::drain(&core).await;
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap().len(),
            1,
            "and it is a reminder"
        );
    }

    #[tokio::test]
    async fn the_undo_is_offered_only_where_the_reminder_can_actually_come_back() {
        use crate::store::artifacts::NewArtifact;
        let core = test_core().await;
        let out = core
            .ingest("erinnere mich Freitag, die Rechnung zu senden", "web", None)
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let read = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.in_results())
            .expect("a live artifact");
        assert!(
            core.can_be_a_reminder(&read.id).await.unwrap(),
            "one window, read from a note: there is something to hand back"
        );

        let src = core
            .store
            .insert_corpus("a note nothing segmented", "web", None)
            .await
            .unwrap();
        let windowless = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a note nothing segmented".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        assert!(
            core.store
                .get_artifact(&windowless)
                .await
                .unwrap()
                .corpus_id
                .is_some(),
            "the old predicate would have offered the button here"
        );
        assert!(
            !core.can_be_a_reminder(&windowless).await.unwrap(),
            "and pressing it would have restored nothing"
        );
    }

    #[tokio::test]
    async fn a_buried_merge_source_restored_from_insights_stays_restored() {
        use crate::store::artifacts::{ArtifactStatus, NewMerged};
        let core = test_core().await;
        let out = core
            .ingest("the roof gutters and what they cost", "web", None)
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let root = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.in_results())
            .expect("a live artifact")
            .id;
        let merged = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "the merged account of the gutters".into(),
                    title: Some("gutters".into()),
                    ..Default::default()
                },
                std::slice::from_ref(&root),
            )
            .await
            .unwrap();
        assert_eq!(
            core.store.roots_to_hide(&merged.id).await.unwrap(),
            vec![root.clone()],
            "the merge may hide its source, to begin with"
        );

        core.store
            .set_superseded_by(&root, Some(&merged.id))
            .await
            .unwrap();
        core.store
            .set_artifact_status(&root, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - 1_000,
            &root
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET created_at = ? WHERE id = ?",
            crate::store::now() - 1_000,
            &merged.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        core.store
            .bury(
                &root,
                "{}",
                100,
                None,
                None,
                &crate::jobs::reap::test_support::row(&root),
            )
            .await
            .unwrap();
        assert!(
            core.store
                .get_artifact(&root)
                .await
                .unwrap()
                .reaped_at
                .is_some(),
            "the fixture must actually be buried"
        );

        core.reactivate(&root).await.unwrap();
        let back = core.store.get_artifact(&root).await.unwrap();
        assert!(back.reaped_at.is_none() && back.superseded_by.is_none());
        assert!(
            core.store
                .roots_to_hide(&merged.id)
                .await
                .unwrap()
                .is_empty(),
            "and the repair may never hide it again"
        );
        let dirty: i64 =
            sqlx::query_scalar!("SELECT lifecycle_dirty FROM artifacts WHERE id = ?", &root)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(
            dirty, 0,
            "the point arrives with the embed, payload and all"
        );
    }

    #[tokio::test]
    async fn a_forced_reminder_is_never_flagged_beside_what_it_resembles() {
        let core = test_core().await;
        let first: String = (1..=30)
            .map(|n| format!("Schritt {n}: die Unterlagen für den Termin durchsehen.\n"))
            .collect();
        core.ingest(&first, "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let again = first.replacen("Schritt 7:", "Schritt sieben:", 1);
        let again = again.as_str();
        // Read before the capture, or the row this call stores answers.
        assert!(
            core.store
                .find_near_duplicate(
                    &crate::store::shingle::signature(again),
                    core.consolidate.near_dupe_min,
                )
                .await
                .unwrap()
                .is_some(),
            "the fixture must actually resemble the first capture"
        );
        let out = core
            .ingest_capture(
                Capture::new(again, "cli").with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .unwrap();
        assert!(
            out.near_duplicate.is_none(),
            "nothing was flagged, so nothing is reported as flagged"
        );
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .near_dupe_of
                .is_none(),
            "a forced reminder is not flagged"
        );
        crate::jobs::test_support::drain(&core).await;
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap().len(),
            1,
            "the reminder the door asked for was armed"
        );
    }

    #[tokio::test]
    async fn a_reminder_on_text_already_captured_is_armed_on_the_stored_row() {
        let core = test_core().await;
        let text = "Rechnung am Freitag verschicken.";
        let first = core.ingest(text, "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "the first capture named no intent, so it armed nothing"
        );

        let again = core
            .ingest_capture(
                Capture::new(text, "cli").with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .unwrap();
        assert_eq!(again.id, first.id, "the stored row is the right row");
        assert!(again.duplicate);
        assert_eq!(
            core.store.get_corpus(&first.id).await.unwrap().metadata["intent"],
            "remind",
            "the door's word is written down"
        );
        crate::jobs::test_support::drain(&core).await;
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap().len(),
            1,
            "the judged read ran again and armed the reminder"
        );
    }

    #[tokio::test]
    async fn is_a_reminder_arms_nothing_on_a_capture_read_window_by_window() {
        let core = test_core().await;
        let long = "Der Bericht behandelt die Migration im Detail. ".repeat(400);
        let out = core.ingest(&long, "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        assert!(
            core.store.segments_for_corpus(&out.id).await.unwrap().len() > 1,
            "the fixture must actually split"
        );
        let art = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        let aid = art
            .first()
            .expect("the capture produced artifacts")
            .id
            .clone();
        assert!(
            !core.set_reminder(&aid, true).await.unwrap(),
            "nothing was armed, and the answer says so"
        );
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "and no window was sent back to be re-read"
        );
    }

    #[tokio::test]
    async fn a_reminder_too_large_to_be_judged_is_refused_at_the_door() {
        let core = test_core().await;
        let long = "Der Bericht behandelt die Migration im Detail. ".repeat(400);
        let err = core
            .ingest_capture(
                Capture::new(&long, "ui").with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .expect_err("a manual is not a reminder");
        let msg = err.to_string();
        assert!(
            msg.contains("engram -c"),
            "it names the verb that works: {msg}"
        );
        assert!(
            core.store
                .find_by_hash(&crate::store::corpora::content_hash(&long))
                .await
                .unwrap()
                .is_none(),
            "nothing stored, nothing embedded"
        );

        core.ingest_capture(Capture::new(&long, "ui"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_reminder_with_no_synthesizer_is_refused_as_not_configured() {
        let core = crate::core::test_support::test_core_without_synthesizer().await;
        let text = "Call the plumber on Friday.";
        let err = core
            .ingest_capture(
                Capture::new(text, "ui").with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .expect_err("nothing can read a reminder");
        assert!(err.to_string().contains("not configured"), "{err}");
        assert!(
            core.store
                .find_by_hash(&crate::store::corpora::content_hash(text))
                .await
                .unwrap()
                .is_none()
        );
        core.ingest_capture(Capture::new(text, "ui")).await.unwrap();
    }

    #[tokio::test]
    async fn superseding_an_artifact_carries_the_reminder_standing_on_it() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("the gutters", [1.0, 0.0]), ("the drain", [0.0, 1.0])],
        )
        .await;
        let (loser, winner) = (ids[0].clone(), ids[1].clone());

        let at = crate::store::now() + 7 * 86_400;
        core.store
            .insert_moment(&NewMoment {
                artifact_id: loser.clone(),
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1);

        core.supersede(&loser, &winner).await.unwrap();

        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1, "the reminder is still on the band: {open:?}");
        assert_eq!(open[0].moment.artifact_id, winner, "it moved to the winner");
        assert_eq!(open[0].moment.at, Some(at));
    }

    #[tokio::test]
    async fn a_carried_moment_never_doubles_an_instant_the_winner_holds() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("the gutters", [1.0, 0.0]), ("the drain", [0.0, 1.0])],
        )
        .await;
        let (loser, winner) = (ids[0].clone(), ids[1].clone());
        let at = crate::store::now() + 7 * 86_400;
        for id in [&loser, &winner] {
            core.store
                .insert_moment(&NewMoment {
                    artifact_id: id.clone(),
                    kind: Kind::Due,
                    at: Some(at),
                    tz: "Europe/Berlin".into(),
                    rule: None,
                    source: Source::Classified,
                    span: None,
                    series_id: None,
                })
                .await
                .unwrap();
        }
        core.supersede(&loser, &winner).await.unwrap();
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1, "one row, not two: {open:?}");
        assert_eq!(open[0].moment.artifact_id, winner);
    }

    #[tokio::test]
    async fn the_zone_a_door_sent_lands_in_metadata() {
        let core = test_core().await;
        let out = core
            .ingest_capture(Capture::new("x", "ui").with_tz(Some("Europe/Berlin".into())))
            .await
            .unwrap();
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().metadata["tz"],
            "Europe/Berlin"
        );
    }
}

#[cfg(test)]
mod decode_text_tests {
    use super::{NotText, decode_text};

    const LINE: &str = "Du hast eine Datei an Ronny gesendet — schön";

    fn utf16(le: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            out.extend(if le { [0xFF, 0xFE] } else { [0xFE, 0xFF] });
        }
        for u in LINE.encode_utf16() {
            out.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    #[test]
    fn utf8_is_taken_as_it_is() {
        assert_eq!(decode_text(LINE.as_bytes().to_vec()).as_deref(), Ok(LINE));
    }

    #[test]
    fn utf16_with_a_byte_order_mark_is_decoded_either_way_round() {
        assert_eq!(decode_text(utf16(true, true)).as_deref(), Ok(LINE));
        assert_eq!(decode_text(utf16(false, true)).as_deref(), Ok(LINE));
    }

    #[test]
    fn utf16_without_a_byte_order_mark_is_refused_not_stored() {
        let ascii: Vec<u8> = "Du hast"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(
            std::str::from_utf8(&ascii).is_ok(),
            "the premise: it passes as UTF-8"
        );
        assert_eq!(decode_text(ascii), Err(NotText::Nul));
    }

    #[test]
    fn bytes_that_are_no_text_at_all_are_still_refused() {
        assert_eq!(decode_text(vec![0xC3, 0x28]), Err(NotText::Undecodable));
        // A byte-order mark followed by an unpaired surrogate.
        assert_eq!(
            decode_text(vec![0xFF, 0xFE, 0x00, 0xD8]),
            Err(NotText::Undecodable)
        );
    }
}
