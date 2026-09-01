use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::corpora::{
    Corpus, CorpusStatus, Followup, Insertion, NearDuplicate, content_hash,
};
use crate::store::jobs::Stage;
use crate::store::now;

/// The channel an image arrives through. Its own value, like `upload`, so
/// the queue and the detail page can tell a photo from a paste.
pub const ORIGIN_IMAGE: &str = "image";
/// The channel a PDF arrives through. Its own value for the same reason a
/// photo has one: the queue and the detail page have to tell a document that
/// was extracted from one that was typed.
pub const ORIGIN_PDF: &str = "pdf";
/// Text typed or pasted into the capture box.
pub const ORIGIN_WEB: &str = "web";
/// Written as a diary entry: the day page shows it in full, oldest first.
pub const ORIGIN_JOURNAL: &str = "journal";
/// A page read from a pasted link, by this server, as a stranger.
pub const ORIGIN_FETCH: &str = "fetch";
/// A share from a phone's share sheet — the Android share target, the iOS
/// Shortcut and the bookmarklet alike. One value for all three, because the
/// distinction between them is one the operator cannot act on.
pub const ORIGIN_SHARE: &str = "share";
/// An answer the operator chose to keep. Its own value because a corpus whose
/// text a model wrote must never read as one a person typed — that difference
/// is the whole of what the keep-this-answer door concedes, and a bare literal
/// in one handler is not where a distinction that load-bearing should live.
pub const ORIGIN_ASK: &str = "ask";
/// Bytes a file may weigh to be stored as text. The kind-specific ceilings
/// are configuration because a photo and a book differ by an order of
/// magnitude and an operator has to be able to say so; this one is not,
/// because there is no such thing as a text file that large on purpose.
///
/// The same figure as `web::MAX_BODY_BYTES` and deliberately not that
/// constant: that one bounds any request body whatever it holds, this one
/// bounds one artifact however it arrived — through `/capture`, a share
/// sheet, `/mcp` or a shell.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Longest note spent on a vision call. Context, not a document: the note is
/// the lead line of the describe prompt, and an unbounded one swamps the
/// description or overruns the request.
///
/// It bounds that copy and nothing else. A note is stored whole — it is an
/// artifact like any other text, and `jobs::embed` cuts an oversize chunk into
/// siblings rather than losing the tail of it. Truncating on the way in was a
/// silent amputation with no receipt anywhere: the operator saw a note go in
/// and had no way to learn that most of it had not.
pub const MAX_NOTE_CHARS: usize = 2000;

/// The user's context for a capture, cleaned: trimmed, `None` when there is
/// nothing in it.
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
    /// True when the text was already stored byte for byte and no new source
    /// was created.
    pub duplicate: bool,
    /// Set when the text is not identical to anything stored but is close
    /// enough that segmenting it would produce artifacts competing with ones
    /// that already exist. The capture is stored and parked, never dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_duplicate: Option<NearDuplicate>,
}

impl IngestOutcome {
    /// The answer for text or bytes already stored: the corpus that has them.
    fn existing(c: &Corpus) -> Self {
        IngestOutcome {
            id: c.id.clone(),
            status: c.status,
            duplicate: true,
            near_duplicate: None,
        }
    }
}

/// What an operator decided about a parked capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NearDupeAction {
    /// The new capture is the better copy: delete the old corpus and its
    /// artifacts, then process this one.
    Replace,
    /// They are genuinely different despite the score. Process this one and
    /// leave the other alone.
    KeepBoth,
    /// The new capture adds nothing. Delete it.
    Discard,
}

/// One PDF, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct PdfCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
}

/// One image, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct ImageCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
}

/// One capture, whichever door it arrived through.
///
/// A struct rather than four positional arguments: `origin` and `source_url`
/// are two different facts about the same event, and every existing caller
/// that has nothing to say about the second should not have to say `None`.
#[derive(Debug, Clone)]
pub struct Capture {
    pub text: String,
    /// The channel: `web`, `mcp`, `extension`, `fetch`, `upload`.
    pub origin: String,
    pub title_hint: Option<String>,
    /// Where the text was read. Provenance, never an instruction: nothing
    /// downstream ever fetches this.
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

    /// The IANA zone the door was in, so a date read out of the text is
    /// resolved where it was written.
    pub fn with_tz(mut self, tz: Option<String>) -> Self {
        if let Some(t) = tz.filter(|t| !t.trim().is_empty()) {
            self.metadata["tz"] = serde_json::Value::String(t);
        }
        self
    }

    /// A door that already knows this is a reminder (`engram -r`, `?intent=`)
    /// says so, and the stage skips the classifier.
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

    /// The `ask` facts of an answer the operator chose to keep: which question
    /// it answered, and which artifacts it was written from.
    ///
    /// Provenance, never an instruction — like `source_url`, nothing downstream
    /// reads these to go and do anything. They exist so that a corpus whose
    /// text a model wrote says so, and says what it was written from, however
    /// much the operator edited before saving. Without them a kept answer is
    /// indistinguishable from something a person typed, which is the one thing
    /// this door must not become.
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
}

/// The last path segment of a URL, when it reads as a file name. `plan.pdf`
/// out of `/papers/plan.pdf`; nothing out of `/` or `/papers/`.
fn url_filename(url: &url::Url) -> Option<String> {
    url.path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Core {
    /// Store the text and queue processing. Deliberately makes no inference
    /// call: capture must stay instant and must survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        self.ingest_capture(Capture::new(text, origin).with_title(title_hint.map(str::to_string)))
            .await
    }

    /// Capture whatever a URL holds: a page is extracted and stored as a
    /// `fetch` capture, a PDF or an image is stored for its reading stage.
    /// The link is provenance on the corpus whichever it was.
    ///
    /// Both link doors — paste-a-link and MCP — come through here, so that
    /// what one can read the other can too.
    pub async fn ingest_url(
        &self,
        url: &url::Url,
        title: Option<String>,
        note: Option<String>,
    ) -> Result<IngestOutcome> {
        use crate::core::fetch::Fetched;
        let source_url = Some(url.to_string());
        match crate::core::fetch::fetch(url, &self.capture).await? {
            Fetched::Html(html) => {
                let page = crate::core::extract::extract(
                    html,
                    Some(url.clone()),
                    self.capture.min_extracted_chars,
                )
                .await?;
                // A title the caller gave wins; the page's own is the fallback,
                // ahead of the first heading `derive_title` would otherwise
                // take — which, readability having dropped the `<h1>`, is the
                // first *section* of the article.
                self.ingest_capture(
                    Capture::new(page.markdown, ORIGIN_FETCH)
                        .with_title(title.or(page.title))
                        .with_note(note)
                        .with_source_url(source_url),
                )
                .await
            }
            Fetched::Pdf(bytes) => {
                self.ingest_pdf_from(
                    PdfCapture {
                        bytes,
                        filename: url_filename(url),
                        title_hint: title,
                        note,
                    },
                    source_url,
                )
                .await
            }
            Fetched::Image { bytes, .. } => {
                self.ingest_image_from(
                    ImageCapture {
                        bytes,
                        filename: url_filename(url),
                        title_hint: title,
                        note,
                    },
                    source_url,
                )
                .await
            }
        }
    }

    /// The same thing, for a door that also knows where the text was read.
    pub async fn ingest_capture(&self, c: Capture) -> Result<IngestOutcome> {
        // A door that says "this is a journal entry" is taken at its word,
        // where a person typed it. Everything unforced waits for the judged
        // synthesis call — the cue table retired with the classifier.
        let mut c = c;
        let journal = crate::core::moments::Intent::Journal;
        let is_entry = c.metadata["intent"].as_str() == Some(journal.as_str());
        if is_entry && crate::jobs::judgement::JOURNALABLE.contains(&c.origin.as_str()) {
            let was = std::mem::replace(&mut c.origin, ORIGIN_JOURNAL.to_string());
            c.metadata["origin_was"] = serde_json::Value::String(was);
        }
        let c = c;
        let text = c.text.as_str();
        let origin = c.origin.as_str();
        let title_hint = c.title_hint.as_deref();
        if text.trim().is_empty() {
            return Err(Error::Validation("text is empty".into()));
        }

        if let Some(existing) = self.store.find_by_hash(&content_hash(text)).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate ingest, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }

        // Computed once, before the insert, so the same signature answers "is
        // this a near-duplicate" and becomes the row's stored column.
        let sig = crate::store::shingle::signature(text);
        let near = self
            .store
            .find_near_duplicate(&sig, self.consolidate.near_dupe_min)
            .await?;

        // Parked, or queued. Synthesis is the expensive stage and text that
        // resembles something stored may not deserve it; an operator decides
        // on Ops. Nothing is lost either way — the corpus is stored verbatim
        // like any other. Written with the row, not after it.
        let followup = match &near {
            Some(n) => Followup::Park {
                of: n.corpus_id.clone(),
                similarity: n.similarity,
            },
            None => Followup::Queue(Stage::Synthesize),
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
            // Another capture of the same bytes landed while this one was
            // scanning for near-duplicates. That scan reads every stored
            // signature, so the window is wide enough to hit in practice — a
            // double-submitted form is enough. The other writer owns the row
            // and has already queued whatever it queued; saying "duplicate" is
            // both true and the same answer this call would have given a
            // moment earlier.
            Insertion::Existing(existing) => {
                tracing::info!(corpus_id = %existing.id, "concurrent duplicate ingest, returning the stored source");
                return Ok(IngestOutcome::existing(&existing));
            }
        };

        match &near {
            Some(n) => tracing::info!(
                corpus_id = %src.id,
                near = %n.corpus_id,
                similarity = n.similarity,
                "capture looks like an existing corpus; parked for review"
            ),
            None => tracing::info!(corpus_id = %src.id, origin, bytes = text.len(), "ingested"),
        }

        self.attach_note_artifact(&src.id, c.metadata["note"].as_str())
            .await;

        Ok(IngestOutcome {
            id: src.id,
            status: src.status,
            duplicate: false,
            near_duplicate: near,
        })
    }

    /// The fork every capture reaches once its text is known: parked next to
    /// what it resembles, or queued for synthesis. The status write is here so
    /// no caller can park without saying what it parked beside.
    pub(crate) async fn park_or_queue(
        &self,
        corpus_id: &str,
        near: Option<&crate::store::corpora::NearDuplicate>,
    ) -> Result<()> {
        let followup = match near {
            Some(n) => crate::store::corpora::Followup::Park {
                of: n.corpus_id.clone(),
                similarity: n.similarity,
            },
            None => crate::store::corpora::Followup::Queue(Stage::Synthesize),
        };
        // One transaction, the same one a text capture's insert uses: the
        // status and the job it implies cannot be written apart.
        self.store.apply_followup(corpus_id, followup).await?;
        if let Some(n) = near {
            tracing::info!(
                corpus_id,
                near = %n.corpus_id,
                similarity = n.similarity,
                "looks like an existing corpus; parked for review"
            );
        }
        Ok(())
    }

    /// The operator's sentence about a file, written where it can be found.
    ///
    /// Embedding runs over artifact chunks and never over metadata, so a note
    /// left in `metadata["note"]` is invisible to search — on a PDF or a text
    /// upload absolutely, and on a photograph only as whatever the vision
    /// model happened to echo back.
    ///
    /// `corpus_span: None` is the point: the note is *about* the file and is no
    /// line *of* it, so it claims no span and nothing tries to read it beside
    /// lines it did not come from. `segment_idx: None` puts it ahead of every
    /// window in `renumber_artifacts`, which orders by
    /// `COALESCE(segment_idx, 0), ordinal, rowid` — so it settles at ordinal 0
    /// with no help from either artifact writer.
    ///
    /// `Provenance::Note` is what keeps that survivable. A window-less row
    /// otherwise reads as debris from an older segmentation, and the two
    /// queries that sweep such rows would have deleted the note — or, with a
    /// sentinel index instead, counted it as a window this corpus already owns
    /// and refused to segment the document at all.
    ///
    /// The embed is armed here rather than left to `settle`. A scan with no
    /// text layer parks as `failed` and never reaches settling, and that is
    /// exactly the capture whose note is the only text anyone will ever have.
    /// By the time this runs the corpus row is committed and its stages are
    /// queued, so a store failure here is not a failed capture and must not be
    /// reported as one: the browser would put the file back for a retry, the
    /// retry would find the hash already stored and answer `duplicate` — which
    /// correctly writes no note — and the operator would end up with the file
    /// in the base and the note nowhere.
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

    /// Store the image and queue the vision stage. Like `ingest_capture`, this
    /// makes no inference call: the phone gets its answer the moment the bytes
    /// are safe, and a dead vision endpoint costs a wait, not a photo.
    /// Store the bytes and queue the reading. No gate: extraction is local, so
    /// unlike the image door this one is open whatever `[infer]` holds.
    ///
    /// No decode permit either — there is no pixel work to bound — and no
    /// preview, because rendering a first page needs pdfium and that is the ML
    /// build's dependency, not this one's.
    /// Store a file by reading what its bytes say it is: a PDF, an image, or
    /// UTF-8 text. Nothing else — bytes we cannot read are refused rather than
    /// stored as a corpus nobody can search.
    ///
    /// `origin` is the caller's, because the doors that reach this differ in
    /// the one way a person later cares about: `/mcp` is an agent, `share` is
    /// a phone's share sheet, `web` is the capture box. It used to be a
    /// constant here, which was only ever true because there was one caller.
    ///
    /// The terminal client has no value of its own: it reaches `/capture` over
    /// HTTP like any other client and declares nothing, so a corpus it stored
    /// reads as `web`. Giving it one means a door=... parameter and an
    /// allowlist to keep a client from claiming a door it is not, the way
    /// `Door::from_client` does for search — worth writing when something
    /// reads the distinction, and not before.
    pub async fn ingest_file(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
        title: Option<String>,
        note: Option<String>,
        origin: &str,
    ) -> Result<IngestOutcome> {
        if bytes.starts_with(b"%PDF-") {
            return self
                .ingest_pdf(PdfCapture {
                    bytes,
                    filename,
                    title_hint: title,
                    note,
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
        let text = String::from_utf8(bytes).map_err(|_| {
            Error::Validation(
                "that file is neither a PDF, an image nor UTF-8 text — nothing here reads it"
                    .into(),
            )
        })?;
        self.ingest_capture(
            Capture::new(text, origin)
                .with_title(title)
                .with_note(note)
                .with_file(filename.as_deref(), size, "text/plain"),
        )
        .await
    }

    pub async fn ingest_pdf(&self, c: PdfCapture) -> Result<IngestOutcome> {
        self.ingest_pdf_from(c, None).await
    }

    /// The same, for a PDF read from a URL: the link is provenance on the
    /// corpus, as it is for a fetched page.
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
        } = c;
        // The ceiling, imposed here rather than at each door.
        //
        // Every route that reaches this is layered at `max(pdf_max_bytes,
        // image_max_bytes)` because one route serves both kinds, and a
        // multipart part carries no ceiling of its own — so a 49 MB file part
        // to `/capture` or `/ui/share` used to arrive unchecked no matter what
        // `image_max_bytes` said. Those doors' comments already promised the
        // ingest path re-imposed it; now it does.
        if bytes.len() > self.capture.pdf_max_bytes {
            return Err(Error::Validation(format!(
                "that PDF is over the {} MB limit for a PDF capture",
                self.capture.pdf_max_bytes / (1024 * 1024)
            )));
        }
        // Hashed before anything else touches it: the same PDF sent twice
        // costs one SHA-256 the second time, not an extraction.
        let hash = content_hash(&bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate PDF, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }

        // The same `file` namespace the image door writes, so the corpus page
        // reads both through one path. No width or height: a PDF has pages,
        // and this build does not count them.
        let mut file = serde_json::json!({
            "size": bytes.len(),
            "mime": "application/pdf",
        });
        if let Some(n) = filename.as_deref() {
            file["name"] = serde_json::Value::String(n.to_string());
        }
        let mut metadata = serde_json::json!({ "file": file });
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
            // A file already in the base keeps the note it was captured with.
            // Stacking a second one per re-upload is how a corpus grows a pile
            // of near-identical captions nobody wrote twice on purpose.
            Insertion::Existing(c) => IngestOutcome::existing(&c),
            Insertion::Created(c) => {
                self.attach_note_artifact(&c.id, note.as_deref()).await;
                IngestOutcome {
                    id: c.id,
                    status: c.status,
                    duplicate: false,
                    near_duplicate: None,
                }
            }
        })
    }

    pub async fn ingest_image(&self, c: ImageCapture) -> Result<IngestOutcome> {
        self.ingest_image_from(c, None).await
    }

    /// The same, for an image read from a URL.
    pub async fn ingest_image_from(
        &self,
        c: ImageCapture,
        source_url: Option<String>,
    ) -> Result<IngestOutcome> {
        if self.describer.is_none() {
            return Err(Error::Validation(
                "image capture is not configured — set [infer.vision] to enable it".into(),
            ));
        }
        let ImageCapture {
            bytes,
            filename,
            title_hint,
            note,
        } = c;
        // The ceiling, imposed here rather than at each door — see
        // `ingest_pdf_from`. Checked before the hash and long before the
        // decode permit, so an oversize photo costs neither a SHA-256 nor a
        // walk over its pixels.
        if bytes.len() > self.capture.image_max_bytes {
            return Err(Error::Validation(format!(
                "that image is over the {} MB limit for an image capture",
                self.capture.image_max_bytes / (1024 * 1024)
            )));
        }
        // Hashed and looked up before it is decoded: a photo sent twice costs
        // one SHA-256 the second time, not a full decode and re-encode.
        let hash = content_hash(&bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate image, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }
        // Decoding, EXIF, the preview and its re-encode are a synchronous walk
        // over up to `image_max_bytes` of pixels. Held on a Tokio worker that
        // is seconds during which search, health and the queue poll on that
        // thread all wait; see `web::api::extract` for the same move.
        //
        // Held across the whole decode, and taken before it rather than inside
        // it: the blocking pool is 512 threads deep, so without a permit the
        // only bound on how much of this runs at once is how fast clients can
        // post. See `image::MAX_CONCURRENT_DECODES`.
        let edge = self.capture.image_preview_edge;
        let permit = self
            .decodes
            .acquire()
            .await
            .expect("the decode permit is never closed");
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
        let note = clean_note(note);
        if let Some(n) = &note {
            metadata["note"] = serde_json::Value::String(n.clone());
        }
        // A filename is a file fact, not a name: `photo.jpg` and `image.png`
        // are what a camera and a clipboard call everything. Seeding the title
        // from it would disarm the one stage that can name the capture.

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
        })
    }

    /// Act on a parked capture. Every branch ends with a corpus that is either
    /// in the pipeline or gone; none of them leaves a corpus stuck in
    /// `needs_review` with no way out.
    pub async fn resolve_near_duplicate(
        &self,
        corpus_id: &str,
        action: NearDupeAction,
    ) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let Some(other) = src.near_dupe_of.clone() else {
            return Err(Error::Validation(
                "this corpus is not parked as a near-duplicate".into(),
            ));
        };

        match action {
            NearDupeAction::Discard => {
                self.delete_corpus(&src.id).await?;
                tracing::info!(corpus_id = %src.id, "discarded a near-duplicate capture");
            }
            NearDupeAction::Replace | NearDupeAction::KeepBoth => {
                if action == NearDupeAction::Replace {
                    // The older corpus goes first. If this fails the new one is
                    // still parked, which is a state an operator can retry from;
                    // releasing it first would leave both live on a failure.
                    //
                    // Unless it is already gone: `near_dupe_of` can name a
                    // corpus that has since been deleted — including another
                    // parked capture that was discarded, since a parked corpus
                    // is still matchable. Failing there would leave the only
                    // way out of the queue behind a 404, with nothing on the
                    // page to say that "keep both" is now the same decision.
                    match self.delete_corpus(&other).await {
                        Ok(()) => {
                            tracing::info!(corpus_id = %src.id, replaced = %other, "replaced an older corpus");
                        }
                        Err(Error::NotFound) => {
                            tracing::info!(
                                corpus_id = %src.id,
                                replaced = %other,
                                "the corpus this capture replaces was already deleted"
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                self.store.set_near_dupe(&src.id, None, None).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
            }
        }
        Ok(())
    }

    /// Put a superseded artifact back in search.
    ///
    /// The payload flag first, then the row — the reverse of the order the
    /// sweep wrote them, and for the same reason. The two stores cannot be
    /// written atomically, so the intermediate state has to be one an operator
    /// can act on: with the flag cleared and the row still set, the artifact is
    /// listed on Ops with its restore button and the next press finishes the
    /// job. Clearing the row first loses the only page that offers the undo
    /// while the artifact is still hidden from search.
    pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        self.unsupersede_locked(artifact_id).await
    }

    /// The body of `unsupersede`, entered with `lifecycle_lock` already held —
    /// `reactivate` routes here so a superseded artifact is not locked twice.
    async fn unsupersede_locked(&self, artifact_id: &str) -> Result<()> {
        // Read before clearing: afterwards nothing says who was hiding it.
        let winner = self.store.get_artifact(artifact_id).await?.superseded_by;
        // Marked before either store is touched. This direction writes the
        // payload first, so without it a crash between the two would leave
        // drift no row write ever announced.
        self.store.mark_lifecycle_dirty(artifact_id).await?;
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Active, None)
            .await?;
        self.store.set_superseded_by(artifact_id, None).await?;
        // Both stores agree now, so the marker the row write set has nothing
        // left to describe. Cleared only here, never before the payload write.
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&artifact_id.to_string()))
            .await?;
        // A restore out of a merge is an operator overruling the merge for
        // this one source. Recorded on the lineage, or the sweep's
        // unfinished-merge repair re-hides it on the next tick, every tick.
        if let Some(w) = winner
            && let Ok(wc) = self.store.get_artifact(&w).await
            && wc.provenance == crate::store::artifacts::Provenance::Merged
        {
            self.store.mark_source_restored(artifact_id).await?;
        }
        self.reopen_the_pairs_that_were_waiting_on(artifact_id)
            .await;
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }

    /// Put the artifact's open questions back on the review queue, now that it
    /// is answerable again.
    ///
    /// Restoring the artifact alone is half an undo. Its open pairs were
    /// settled `Stale` when it left results, and `record_pair` is
    /// `INSERT OR IGNORE`, so the sweep re-finds the same two artifacts and
    /// files nothing. Without this, the contradiction an operator restored the
    /// artifact to look at is gone for good, and nothing anywhere says it ever
    /// existed.
    ///
    /// Only the settled rows, and that is the whole set. The other thing a
    /// supersession does to a pair is move it onto the winner, which leaves no
    /// row between these two artifacts at all — so the similarity sweep's
    /// `record_pair` files a fresh one the next time it looks, exactly as it
    /// would for a pair it had never seen. A settled row is the only kind that
    /// blocks that.
    ///
    /// Logged rather than returned, like the follow in `supersede`: the restore
    /// itself has happened by this point, and reporting failure would tell the
    /// caller the artifact is still hidden when it is not. What is left behind
    /// is a `Stale` row, which is what the next restore of either side reopens.
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

    /// Put a promoted window back: its passages active, the artifacts the
    /// promotion wrote deprecated, the segment `verbatim` again — and marked
    /// `no_promote`, so it stays that way. The links copied onto the artifacts
    /// and the activation they were handed stay where they are — both sides
    /// describe the same corpus lines, and the asymmetry is accepted rather
    /// than fixed.
    ///
    /// The mark is what makes this an undo rather than a pause. The passages
    /// keep the activation that armed the promotion, and `maybe_promote` reads
    /// activation at the bump: restoring `verbatim` alone would let the next
    /// open of any restored passage promote the window again, immediately,
    /// leaving the operator with the same promotion and a set of deprecated
    /// artifacts beside it. Only a re-split clears the mark — a window whose
    /// text changed is a different window.
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

    /// Move an already-hidden artifact from one winner to another.
    ///
    /// Not `supersede`, which refuses a side that is not active — and both sides
    /// here are exactly that: the artifact is already superseded, and it is
    /// being re-pointed precisely because its current winner is about to be
    /// hidden too.
    ///
    /// A supersession chain is what this exists to prevent. `A -> B -> C` leaves
    /// the reader who opens A at an artifact that is not in results either, and
    /// nothing in the UI can follow the second hop. The sweep avoids chains by
    /// grouping with union-find before it decides anything; the merge path
    /// cannot, because the group it is collapsing was already partly collapsed
    /// by an earlier merge.
    ///
    /// Row before payload, like `supersede`.
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

    /// Hide `loser_id` in favour of `winner_id`. Row before payload, matching
    /// the sweep's existing auto-supersede order (`jobs::consolidate`): the
    /// intermediate state after a partial failure is one an operator can act
    /// on, since the artifact is already listed on Ops with its `superseded_by`
    /// set, even if the search-side flag has not caught up yet.
    pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // Neither side may be retired. `set_superseded_by` writes
        // `status = 'superseded'` unconditionally, so superseding an artifact
        // an operator deprecated would silently overwrite that decision with
        // one nothing distinguishes from the sweep's own work. And a deprecated
        // *winner* is worse: the loser gets hidden in favour of an artifact
        // that is itself out of results, so the answer disappears entirely.
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
            .set_superseded_by(loser_id, Some(winner_id))
            .await?;
        self.vectors
            .set_lifecycle(loser_id, ArtifactStatus::Superseded, Some(winner_id))
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&loser_id.to_string()))
            .await?;
        // Every other question about the loser now belongs to the winner. Done
        // after the artifact is hidden, and its failure is logged rather than
        // returned: the supersession itself has happened by this point, so
        // reporting failure would tell `jobs::try_supersede` the artifact
        // "stays active" when it does not. What is left behind is stale pairs,
        // which the sweep's repair pass
        // (`jobs::consolidate::follow_supersessions`) moves on the next tick.
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
        tracing::info!(loser_id, winner_id, "superseded an artifact");
        Ok(())
    }

    /// Flag an artifact stale with no specific replacement. Unlike `supersede`,
    /// there is no winning artifact on the other end — an operator judged the
    /// content itself no longer current.
    ///
    /// Row before payload. SQLite is the source of truth, so the state a
    /// partial failure leaves — row deprecated, still in results — is the one
    /// the sweep's drift repair (`jobs::consolidate::repair_lifecycle_drift`)
    /// finishes by pushing the row's status into the payload. Writing the
    /// payload first would instead hide the artifact behind a row that still
    /// says active, and the repair, reading the row, would undo it.
    pub async fn deprecate(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // A superseded artifact is already out of search, and `deprecate` does
        // not clear `superseded_by`, so this would leave a row that is both:
        // listed on Ops under "deprecated" *and* under "hidden as near
        // identical", with a detail page that renders only the supersession
        // branch and therefore never offers the button that undoes it. The
        // payload would carry `status: deprecated, superseded_by: <winner>`, a
        // combination nothing else in the system produces. The Ops page does not
        // render the button in that state, but the route is reachable directly
        // and this is a public API — so the rule lives here, next to
        // `supersede`'s own guard.
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return Err(Error::Validation(format!(
                "cannot deprecate: {id} is already hidden in favour of another artifact; \
                 reactivate it first"
            )));
        }
        self.store
            .set_artifact_status(id, ArtifactStatus::Deprecated)
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

    /// Move an artifact back to active.
    ///
    /// An artifact that was hidden by a supersession is handed to
    /// `unsupersede`, which clears `superseded_by` on both sides. Flipping the
    /// status alone would leave the row pointing at its winner while the
    /// payload no longer does, so Ops would keep listing it as hidden and the
    /// next consolidation sweep would re-apply `Superseded` to the vector
    /// store — undoing the operator without saying so.
    ///
    /// Payload before row, as `unsupersede` does it and for the same reason:
    /// this direction *reveals*, so the intermediate state has to be the
    /// visible one. Clearing the row first would leave the artifact hidden by a
    /// payload nothing on the page explains — off the Ops deprecated list,
    /// because the row now says active, and so out of reach of the very button
    /// that would fix it. With the payload cleared first the artifact is back
    /// in results immediately, still listed on Ops as deprecated, and one more
    /// press finishes the job.
    pub async fn reactivate(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return self.unsupersede_locked(id).await;
        }
        // As in `unsupersede`: payload first, so the marker has to go first.
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
        tracing::info!(artifact_id = id, "reactivated an artifact");
        Ok(())
    }

    /// Stamp an artifact as confirmed accurate now — what search ranking's
    /// recency decay reads.
    ///
    /// Also zeroes `hit_count`: `stale_max_hits` is "retrieved at most this
    /// many times *since*" the last verification, and a lifetime counter would
    /// mean one appearance in a marked search result kept an artifact off the
    /// review list forever.
    /// A failed vector write puts the row back the way it was. Nothing else
    /// reconciles this pair: `repair_lifecycle_drift` only examines artifacts
    /// that are non-active on one side or the other, so an artifact that is
    /// active on both but stamped on only one is invisible to it — it would keep
    /// ranking as stale and keep appearing on the review list while the row
    /// claimed it had just been confirmed, and the operator would have no way to
    /// tell that from a press that simply did not take. Rolling back leaves both
    /// stores saying the same (old) thing and surfaces the error, so pressing
    /// again is a repair rather than a guess.
    pub async fn verify(&self, id: &str) -> Result<()> {
        let at = now();
        let previous = self.store.get_artifact(id).await?.last_verified_at;
        self.store.set_last_verified_at(id, at).await?;
        if let Err(e) = self.vectors.set_last_verified_at(id, at, true).await {
            // A row that never had a stamp has nothing to roll back to, and
            // clearing the column is not available here.
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

    /// Notice when the two stores disagree about which artifacts exist.
    ///
    /// They hold complementary halves of the same artifact and are written
    /// separately, so either can end up with an entry the other lacks: a crash
    /// between the two writes, a restore of one store from a backup taken at a
    /// different moment, an operator pointing a process at the wrong
    /// `store.path`. A row that says `embedded` with no point is sent back
    /// through the pipeline that writes it. A point with no row is only
    /// reported: SQLite is the source of truth, and the fix is to restore both
    /// stores from the same moment, not to rebuild rows out of payloads.
    ///
    /// The point list is read *first* and the row list second, so an artifact
    /// captured while this runs is either absent from the scroll or present in
    /// the newer row list — never mistaken for an orphan.
    pub async fn heal_store_drift(&self) -> Result<()> {
        use std::collections::HashSet;

        let points = self.vectors.all_artifact_ids().await?;
        let rows = self.store.list_all_artifact_ids().await?;
        let embedded = self.store.list_embedded_artifact_ids().await?;

        let has_row: HashSet<&str> = rows.iter().map(String::as_str).collect();
        let has_point: HashSet<&str> = points.iter().map(String::as_str).collect();

        let orphan_points = points
            .iter()
            .filter(|id| !has_row.contains(id.as_str()))
            .count();
        if orphan_points > 0 {
            tracing::warn!(
                orphan_points,
                "the vector store holds artifacts SQLite does not; restore both stores from the same snapshot"
            );
        }

        let mut requeued = 0usize;
        for id in embedded
            .iter()
            .filter(|id| !has_point.contains(id.as_str()))
        {
            // Idle-only. This runs on every sweep, so `enqueue` — which re-arms
            // whatever state it finds — would wind a unit that is failing
            // against a dead endpoint back to zero attempts every tick. Its
            // backoff would never climb, and the queue's backoff is what stands
            // in for a circuit breaker here: see `infer::gate`.
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

    /// Delete an artifact from both stores, on purpose.
    ///
    /// The vector point goes first, so an interrupted delete leaves a row
    /// whose point is gone — which `heal_store_drift` answers by re-embedding,
    /// so the artifact survives intact instead of half-existing, and pressing
    /// delete again finishes it.
    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        self.store.get_artifact(id).await?;
        self.vectors
            .delete_artifacts(std::slice::from_ref(&id.to_string()))
            .await?;
        self.store.delete_artifact(id).await?;
        // This artifact may have been the reason another one is hidden, and a
        // keeper that no longer exists leaves its loser out of search in favour
        // of nothing.
        self.heal_dangling_supersessions().await?;
        tracing::info!(artifact_id = id, "deleted an artifact");
        Ok(())
    }

    /// Put back anything that was hidden in favour of an artifact which has
    /// since been deleted. Cheap — one query, and vector writes only for the
    /// rows it actually frees — so it runs after every deletion.
    ///
    /// Payload before row, exactly as `unsupersede` does it, and for the same
    /// reason. Clearing the rows first would leave every artifact whose vector
    /// write then failed hidden from search with `superseded_by` already NULL:
    /// off the Ops list, past the sweep's self-heal branch — which only repairs
    /// the opposite skew — and unreachable by any button. This runs first in a
    /// sweep, when Qdrant being unavailable is precisely the case at hand.
    ///
    /// One failure does not abandon the rest: each artifact is independent, and
    /// the ones that can be freed should be. The state a failure leaves behind
    /// is the recoverable one, so the next deletion or sweep finishes the job.
    pub(crate) async fn heal_dangling_supersessions(&self) -> Result<()> {
        // Under the lifecycle lock like every other transition: this path
        // reveals payload-first, and interleaving with the sweep's repair —
        // which reads rows and writes payloads — is exactly the sequence that
        // hides an artifact with no marker left to find it by.
        let _guard = self.lifecycle_lock.lock().await;
        let mut first_err = None;
        for id in self.store.dangling_superseded().await? {
            // Marked before the payload write, as `unsupersede` does and for
            // the same reason: this direction writes the payload first, so
            // without it a crash between the two stores would leave drift no
            // row write ever announced. A payload write that fails below
            // leaves the marker standing — correctly: the failed reveal is
            // then findable drift, and the heal retries on the next sweep.
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
            // Cleared here as everywhere else, because `set_superseded_by`
            // raises the marker in the same statement as the change it
            // describes. This path writes the payload first and the row second,
            // so by now the two already agree — and a marker left standing would
            // have the next sweep's `repair_lifecycle_drift` rewrite a payload
            // that is correct, on a base whose stores are in agreement, which is
            // the one condition that function's own comment says would mean a
            // bug somewhere.
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

    /// Everything a rerun of the pipeline from `raw_text` has to forget first.
    /// Shared by re-segment and re-read; the comment on each line is the reason
    /// the line exists.
    async fn forget_derived_work(&self, id: &str) -> Result<()> {
        // Re-segmenting replaces every chunk, so the old vectors and rows go
        // first.
        //
        // Except the note, which is not derived work: nothing a rerun does can
        // produce the sentence a person typed about the document, so deleting
        // it here would make it unsearchable for good while the caption on the
        // page went on claiming otherwise. Its vector went with the corpus, so
        // it is put back in line for a new one — armed here rather than left to
        // `settle`, for the same reason the door arms it: a re-read that parks
        // as `failed` never reaches settling.
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
        // The window rows are the segment job's memory of what it has already
        // done. Leaving them behind means the rerun finds every window `done`,
        // segments nothing, and lands on a source with no chunks at all.
        // Re-windowing is also the point of a reprocess after a model or budget
        // change.
        self.store.clear_segments(id).await?;
        // The measure those windows produced goes with them. It is not just
        // stale: the reconciliation sweep identifies a document whose last
        // window resolved but whose `settle` never ran by having no coverage,
        // so a value left over from the previous run reads as "already
        // finished". A rerun that dies in exactly that window would then be
        // stuck in `segmenting` for good — nothing resolves again to trigger
        // `settle`, and the one sweep that repairs it has been told there is
        // nothing to repair.
        self.store.set_corpus_coverage(id, None).await?;
        // And the units that name those windows, which outlive them. Planning
        // arms idle-only, so a unit still queued from the run being replaced
        // would carry its attempts into the rerun — the person who asked for
        // another try would get a window that gives up after one.
        self.store.delete_window_jobs(id).await?;
        // The title unit is armed once per corpus and never again, so that a
        // document the model will not name stops costing calls. The row is
        // what remembers that, which also means a corpus left unnamed by a
        // transient failure could never be named again — including by the
        // person who noticed and asked for the rerun. An explicit reprocess is
        // exactly the case that rule is not meant to cover.
        self.store.delete_job(Stage::Title, id).await?;
        // Reprocessing a parked capture is a decision to process it, so the
        // park has to be lifted with it. Leaving the flag set means a fully
        // synthesized and embedded corpus sits on the review queue forever,
        // where the discard button now deletes real work.
        self.store.set_near_dupe(id, None, None).await?;
        Ok(())
    }

    /// A channel label with an undo, and the one write to `corpora.origin`
    /// outside insert. `raw_text` is untouched.
    pub async fn set_entry(&self, corpus_id: &str, on: bool) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let mut meta = src.metadata.clone();
        let origin = if on {
            if src.origin == ORIGIN_JOURNAL {
                return Ok(());
            }
            meta["origin_was"] = serde_json::Value::String(src.origin.clone());
            // Turning it on by hand withdraws an earlier refusal, so the stage
            // may file this note again. Only a person reaches here with `true`
            // while the flag stands: `jobs::moments` reads the refusal first
            // and does not call.
            crate::core::moments::allow_intent(&mut meta, crate::core::moments::Intent::Journal);
            ORIGIN_JOURNAL.to_string()
        } else {
            // The mirror of the guard above, and it has to be here for a
            // stronger reason than symmetry: the first undo removes
            // `origin_was`, so a second one — a re-post of the `on=0` form on
            // the capture receipt, a back-navigation — finds nothing to
            // restore and falls through to `web`. That silently rewrote a
            // `cli`, `share` or `extension` capture's channel to one it never
            // came through. A corpus that is not a journal entry has nothing
            // to undo.
            if src.origin != ORIGIN_JOURNAL {
                return Ok(());
            }
            let was = meta["origin_was"].as_str().unwrap_or(ORIGIN_WEB).to_string();
            if let Some(m) = meta.as_object_mut() {
                m.remove("origin_was");
            }
            // The refusal has to outlive the undo, because the reading that
            // filed the note does not stop being true. `jobs::moments` derives
            // the intent again on every re-embed and would file it a second
            // time — a reindex or a switched embed model quietly overruling the
            // operator, on a note they had already put back.
            crate::core::moments::refuse_intent(&mut meta, crate::core::moments::Intent::Journal);
            was
        };
        self.store.set_corpus_metadata(corpus_id, &meta).await?;
        self.store.set_corpus_origin(corpus_id, &origin).await
    }

    /// The reminder's half of `set_entry`: "this is not a reminder", with an
    /// undo, and it sticks.
    ///
    /// The journal side has had a durable refusal for a while; the reminder
    /// side had none, and it is the side where being wrong costs more — a
    /// journal entry that should not be one is a changed label, while a
    /// reminder that should not be one is a row on the band, an armed unit and
    /// a push to somebody's phone. The only way to make one stay gone across a
    /// re-embed was to mark it *done*, which is the wrong verb for a thing
    /// that was never a task and the wrong record to leave in the base.
    ///
    /// `off` deletes the row only if the stage is what wrote it and nobody has
    /// acted on it since; `on` withdraws the refusal and hands the artifact
    /// back to the stage, which reads the note again exactly as it did the
    /// first time. Nothing here rewrites text.
    pub async fn set_reminder(&self, artifact_id: &str, on: bool) -> Result<()> {
        let art = self.store.get_artifact(artifact_id).await?;
        let Some(cid) = art.corpus_id.as_deref() else { return Ok(()) };
        let src = self.store.get_corpus(cid).await?;
        let mut meta = src.metadata.clone();
        let intent = crate::core::moments::Intent::Remind;
        if on {
            crate::core::moments::allow_intent(&mut meta, intent);
            self.store.set_corpus_metadata(cid, &meta).await?;
            // Hand the note back to the judged read: re-run its window's
            // synthesis, which is where time is read since the reshape.
            if let Some(idx) = art.segment_idx {
                self.store.reset_segment(cid, idx, true).await?;
                self.store
                    .enqueue_seq(
                        Stage::SegmentWindow,
                        "segment",
                        &crate::jobs::window::unit_target(cid, idx),
                        idx,
                    )
                    .await?;
            }
            return Ok(());
        }
        crate::core::moments::refuse_intent(&mut meta, intent);
        self.store.set_corpus_metadata(cid, &meta).await?;
        self.store.delete_read_due(artifact_id).await?;
        self.store.rearm_remind().await
    }

    pub async fn reprocess(&self, id: &str, stage: Stage) -> Result<()> {
        let src = self.store.get_corpus(id).await?;
        match stage {
            Stage::Synthesize | Stage::Enrich => {
                // Re-segmenting starts from `raw_text`, and an image whose read
                // has not landed has none. Flipping it to `raw` would have
                // synthesis fail on empty text and the pending read then find
                // a corpus that is no longer `describing` — a photo never
                // read, by way of a button that promised to process it.
                if src.status == CorpusStatus::Describing
                    || (src.origin == ORIGIN_IMAGE && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this image has not been read yet — re-read it instead".into(),
                    ));
                }
                // Same situation, same answer: re-segmenting starts from
                // `raw_text`, and a PDF whose extraction has not landed has
                // none.
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
                // The artifacts just deleted may have been the surviving half of
                // a consolidated pair; whatever they hid comes back.
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
            // Consolidation and association both look at the whole collection,
            // so there is no such thing as reprocessing one corpus through
            // either. Saying so beats silently queueing a sweep the caller did
            // not ask for.
            Stage::Consolidate | Stage::Associate => {
                return Err(Error::Validation(
                    "that stage is a collection-wide sweep, not a per-corpus stage".into(),
                ));
            }
            // Units the queue arms for itself, one per artifact or inference
            // call. An operator reprocesses a document, not one of its windows:
            // asking for `synthesize` re-windows the whole thing and arms them
            // all, and `embed` re-arms a `relate` unit per artifact behind it.
            Stage::SegmentWindow
            | Stage::Title
            | Stage::Dedupe
            | Stage::Relate
            | Stage::LinkJudge
            | Stage::Generate => {
                return Err(Error::Validation(
                    "that stage is a single inference call the queue arms itself; \
                     reprocess the document instead"
                        .into(),
                ));
            }
            // The pursuit sweep looks at every recorded search, not at one
            // corpus; retention, dedupe arming and the context sweep look at
            // the whole collection for the same reason.
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
            // A stored PDF can always be read again — with the ML build, or
            // after a docling upgrade. The extraction and everything derived
            // from it are replaced wholesale, because an artifact of the old
            // reading has no span in the new one.
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
            // A stored image can always be read again — with a better model, or
            // after the endpoint that refused it is fixed. The reading and
            // everything derived from it are replaced wholesale, because a
            // chunk of the old reading has no span in the new one.
            Stage::Describe => {
                // Origin, not just "has an attachment": a PDF has one too, and
                // sending one through here would wipe its extraction and hand
                // the vision model a preview it does not have.
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

#[cfg(test)]
mod tests {

    use crate::core::ingest::MAX_TEXT_BYTES;
    use crate::core::ingest::{
        Capture, ImageCapture, MAX_NOTE_CHARS, NearDupeAction, ORIGIN_FETCH, ORIGIN_IMAGE,
        ORIGIN_PDF, PdfCapture,
    };
    use crate::core::test_support::test_core;
    use crate::error::Error;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;

    /// The per-kind ceilings, imposed on the path rather than at each door.
    ///
    /// The doors are layered at `max(pdf_max_bytes, image_max_bytes)` because
    /// one route serves both kinds, and a multipart part carries no ceiling of
    /// its own — so `/capture` and `/ui/share` both used to walk a 49 MB photo
    /// through a full decode however small `image_max_bytes` was set.
    #[tokio::test]
    async fn a_file_over_its_kind_s_ceiling_is_refused() {
        let mut core = test_core().await;
        core.capture.image_max_bytes = 1024 * 1024;
        core.capture.pdf_max_bytes = 2 * 1024 * 1024;

        // PNG magic and then padding: never a decodable image, which is the
        // assertion. The ceiling is checked before the hash and long before
        // the decode permit, so these bytes are refused on their weight alone.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(2 * 1024 * 1024, 0);
        let err = core
            .ingest_file(png, Some("photo.png".into()), None, None, "share")
            .await
            .expect_err("an oversize image is refused");
        assert!(err.to_string().contains("1 MB limit"), "{err}");

        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(3 * 1024 * 1024, 0);
        let err = core
            .ingest_file(pdf, Some("book.pdf".into()), None, None, "share")
            .await
            .expect_err("an oversize PDF is refused");
        assert!(err.to_string().contains("2 MB limit"), "{err}");

        // And text, whose ceiling is not configuration: `/corpora/upload`
        // refuses text over 8 MB and a `file` part used to sail past it.
        let text = vec![b'a'; MAX_TEXT_BYTES + 1];
        let err = core
            .ingest_file(text, Some("notes.txt".into()), None, None, "share")
            .await
            .expect_err("an oversize text file is refused");
        assert!(err.to_string().contains("8 MB limit"), "{err}");
    }

    /// The two link doors share one path, and the path decides by what the
    /// URL held: a page is extracted here, a PDF is stored for `Stage::Extract`.
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
            .ingest_url(&u, None, Some("from the agent".into()))
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
        let out = core.ingest_url(&u, None, None).await.unwrap();
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
        // Unlike the image door, which refuses without `[infer.vision]`:
        // extraction is local, so nothing gates it.
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
            )
            .await
            .expect("image file");
        assert_eq!(
            png.status,
            CorpusStatus::Describing,
            "an image is read by a job"
        );

        let refused = core
            .ingest_file(vec![0xff, 0xfe, 0x00], None, None, None, "cli")
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
        }
    }

    /// `MAX_IMAGE_EDGE` and `MAX_DECODE_BYTES` bound one image and say nothing
    /// about ten at once. Decoding runs on a 512-thread blocking pool through
    /// no gate at all, so without a permit the ceiling on resident pixels is
    /// however many uploads a client cares to have in flight.
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
        // And once they are free again it goes through as usual.
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
        // Still describing.
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
        // Failed before any text was read.
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400")
            .await
            .unwrap();
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
        // The describe job and the status are untouched either way.
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

    /// `describe` is guarded by origin, not by "has an attachment": a PDF has
    /// one too, and letting it through would clear the extraction and every
    /// artifact behind it before handing the vision model a preview a PDF is
    /// stored without.
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
        // `set_superseded_by` raises `lifecycle_dirty` in the same statement as
        // the change it describes, and every other caller clears it once the
        // payload write is acknowledged. This path writes the payload *first*,
        // so the two stores already agree by the time the row is written — and a
        // marker left standing has the next sweep's `repair_lifecycle_drift`
        // rewrite a payload that is already correct, on a base in agreement.
        // That function returns a count precisely so that a repair firing on
        // such a base is visible rather than hiding behind a correct end state.
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
        // The channel and the location are two different facts. Overloading
        // one with the other loses the channel and leaves the URL unqueryable.
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

    /// A body long enough to have a stable shingle signature.
    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn reprocessing_forgets_the_previous_runs_coverage() {
        // The reconciliation sweep identifies a document whose last window
        // resolved but whose `settle` never ran by its having no coverage. A
        // reprocess that left the previous run's measure behind made its own
        // rerun the one case the repair could not see: if that rerun died in
        // exactly that window, nothing would resolve again to trigger `settle`,
        // and the sweep would read the stale number as "already finished". The
        // document sits in `segmenting` for good — never renumbered, never
        // embedded, never in search.
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
        // Planning arms idle-only now, so the window units are the one piece of
        // the previous run that a reprocess would otherwise inherit: a rerun
        // asked for by a person would start its windows four attempts in and
        // give up on them almost at once. `clear_segments` drops the rows the
        // units name but not the units, which outlive them.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
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
        // A title unit is armed once per corpus and never again, so that a
        // document the model will not name stops costing four calls a day
        // forever. The row is what remembers that — and it outlived a
        // reprocess, so a corpus left unnamed by an endpoint that was briefly
        // down could never be named again, including by the person who noticed
        // and asked for the rerun.
        let core = test_core().await;
        let src = core.ingest(&manual("mount"), "web", None).await.unwrap();
        for _ in 0..200 {
            sqlx::query("UPDATE jobs SET run_after = 0 WHERE state = 'pending'")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(&core).await.unwrap_or(false) {
                break;
            }
        }
        // Capture names locally now, so the title unit is armed by putting
        // the corpus in the state the unit exists for: unnamed at settle.
        sqlx::query("UPDATE corpora SET title_hint = NULL WHERE id = ?")
            .bind(&src.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        crate::jobs::synthesize::finish(&core, &src.id).await.unwrap();
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
    async fn a_near_identical_capture_is_parked_rather_than_synthesised() {
        // The whole point: a re-pasted chapter must not cost a model call, and
        // must not become a second set of artifacts competing with the first.
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let edited = manual("mount").replacen("step 7:", "step seven:", 1);
        let second = core.ingest(&edited, "web", None).await.unwrap();

        assert_ne!(second.id, first.id, "the capture must still be stored");
        assert!(!second.duplicate, "it is not a byte-identical duplicate");
        assert_eq!(second.status, CorpusStatus::NeedsReview);
        let near = second.near_duplicate.expect("no near-duplicate reported");
        assert_eq!(near.corpus_id, first.id);
        assert!(near.similarity > 0.90);
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a parked capture must not queue synthesis"
        );
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
    async fn keeping_both_releases_the_capture_into_the_pipeline() {
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

        core.resolve_near_duplicate(&second.id, NearDupeAction::KeepBoth)
            .await
            .unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert!(got.near_dupe_of.is_none(), "the flag must be cleared");
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn replacing_deletes_the_older_corpus_and_its_vectors() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert!(core.vectors.count().await.unwrap() > 0);

        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        core.resolve_near_duplicate(&second.id, NearDupeAction::Replace)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&first.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::Raw
        );
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn discarding_removes_the_new_capture_only() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();

        core.resolve_near_duplicate(&second.id, NearDupeAction::Discard)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&second.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert!(core.store.get_corpus(&first.id).await.is_ok());
    }

    #[tokio::test]
    async fn replacing_a_corpus_that_is_already_gone_still_releases_the_capture() {
        // `near_dupe_of` can name a corpus that has since been deleted —
        // including another parked capture that was discarded, since a parked
        // corpus is still matchable. Failing here put the only way out of the
        // review queue behind a 404.
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        core.delete_corpus(&first.id).await.unwrap();

        core.resolve_near_duplicate(&second.id, NearDupeAction::Replace)
            .await
            .unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert!(got.near_dupe_of.is_none());
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn reprocessing_a_parked_capture_takes_it_off_the_review_queue() {
        // Reprocessing is a decision to process. Leaving the park set left a
        // fully synthesized corpus on the queue where "discard" deletes it.
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
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::NeedsReview
        );

        core.reprocess(&second.id, Stage::Synthesize).await.unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert!(
            got.near_dupe_of.is_none(),
            "the capture is being processed and still asks to be decided on"
        );
        assert_eq!(got.status, CorpusStatus::Raw);
    }

    #[tokio::test]
    async fn resolving_a_corpus_that_is_not_parked_is_rejected() {
        let core = test_core().await;
        let out = core.ingest("ordinary text", "web", None).await.unwrap();
        assert!(matches!(
            core.resolve_near_duplicate(&out.id, NearDupeAction::Replace)
                .await,
            Err(crate::error::Error::Validation(_))
        ));
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
        // The whole point of deferred processing: a broken inference endpoint
        // must not turn into a failed capture.
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(
            crate::infer::fake::FakeSynthesizer::failing("endpoint down"),
        ) ;
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
        // The duplicate check and the insert are separated by a scan over every
        // stored signature, so a double-submitted form is enough to have both
        // calls pass the check. The loser used to hit the UNIQUE constraint and
        // surface as a 500 on a capture that was, in fact, a duplicate.
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
                    ordinal: 0,
                    text: "t".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
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
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                    hit_count: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                    origin_corpora: vec![],
                    provenance: None,
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
        // The window rows say what the segment job has already finished.
        // Keeping them across a reprocess left the rerun with nothing pending,
        // no chunks, and a source marked failed.
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

    /// One point for `artifact_id`, with nothing set beyond what a write needs.
    fn point(artifact_id: &str, corpus_id: &str) -> crate::vector::VectorPoint {
        crate::vector::VectorPoint {
            sparse: Default::default(),
            vector: vec![0.1; 8],
            payload: crate::vector::VectorPayload {
                artifact_id: artifact_id.to_string(),
                corpus_id: corpus_id.to_string(),
                text: "t".into(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                hit_count: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
                provenance: None,
            },
        }
    }

    /// A corpus with one artifact in it, and the ids of both.
    async fn one_artifact(core: &crate::core::Core) -> (String, String) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "t".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        (src.id, made[0].id.clone())
    }

    #[tokio::test]
    async fn deprecating_an_already_superseded_artifact_is_refused() {
        // `set_artifact_status` leaves `superseded_by` alone, so this used to
        // produce a row that is deprecated *and* hidden in favour of a winner:
        // listed in both Ops tables, rendered only as a supersession, and so out
        // of reach of the button that would undo it.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "loser".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "winner".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
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
        // The other direction. A row still `pending` is not drift — that is
        // every artifact that was just captured — so only a row claiming `done`
        // counts, and the repair is to send it back through the pipeline that
        // writes points rather than to delete the row.
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
        // The sweep runs this every tick, and a unit that is failing against a
        // dead endpoint is `pending` with `run_after` in the future. Re-arming
        // it unconditionally winds `attempts` back to zero, so the backoff
        // never climbs and the sweep hands the same dead endpoint another
        // full-timeout call on every tick, forever. The queue's backoff is the
        // only thing standing in for the circuit breaker, and this is what
        // used to reset it.
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
            sqlx::query_as("SELECT attempts, run_after FROM jobs WHERE id = ?")
                .bind(job.id)
                .fetch_one(&core.store.control.pool)
                .await
                .unwrap();
        assert!(before.0 > 0 && before.1 > 0, "the unit is not backing off");

        core.heal_store_drift().await.unwrap();

        let after: (i64, i64) = sqlx::query_as("SELECT attempts, run_after FROM jobs WHERE id = ?")
            .bind(job.id)
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(
            after, before,
            "the drift repair cleared a retrying unit's backoff"
        );
    }

    #[tokio::test]
    async fn a_point_whose_row_is_gone_is_reported_and_left_alone() {
        // SQLite is the source of truth; a point with no row is drift to be
        // restored from a snapshot, not a row to be rebuilt from a payload —
        // and never a point to delete, since the vectors may be the last copy.
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
        // Everything just ingested has a row and no point. Treating that as
        // drift would re-queue the entire backlog on every sweep.
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
    async fn without_a_vision_role_the_image_door_is_closed() {
        let core = crate::core::test_support::test_core_without_vision().await;
        let e = core
            .ingest_image(ImageCapture {
                bytes: a_seeded_png(7),
                filename: None,
                title_hint: None,
                note: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(e.to_string().contains("not configured"), "{e}");
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
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
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    /// A note is no longer truncated on the way into storage. It is an
    /// artifact like any other, and `embed::run_with_limit` splits an oversize
    /// chunk into siblings — so length is the embedder's problem, not a silent
    /// amputation at the door.
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
            }),
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
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

    /// The sentence most worth searching on must be an artifact, not a
    /// caption. Embedding runs over artifact chunks and never over metadata,
    /// so a note that stays in `metadata["note"]` cannot be found at all.
    #[tokio::test]
    async fn a_note_on_a_pdf_becomes_a_span_less_artifact_on_its_corpus() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("lease.pdf".into()),
                title_hint: None,
                note: Some("  scan of the Reinhardt lease, break clause is p.3  ".into()),
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

    /// One helper, three doors, so a fourth cannot forget it.
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

    /// A scan with no text layer parks as `failed` and never reaches `settle`,
    /// which is what normally arms the embed. Without arming it here, the one
    /// thing a person typed about an unreadable document waits forever.
    #[tokio::test]
    async fn a_note_arms_the_embed_so_a_parked_capture_still_becomes_findable() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("scan.pdf".into()),
                title_hint: None,
                note: Some("the survey nobody can OCR".into()),
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

    /// Re-uploading the same file must not stack a second note on it.
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
            })
            .await
            .unwrap();
        let again = core
            .ingest_pdf(PdfCapture {
                bytes,
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("a second thought about it".into()),
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

    /// A note is not derived work. A reprocess replaces everything the pipeline
    /// made from the document; the sentence a person typed about it is the one
    /// thing in the corpus no rerun can produce again.
    #[tokio::test]
    async fn a_reprocess_keeps_the_note_and_queues_it_for_a_fresh_vector() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("lease.pdf".into()),
                title_hint: None,
                note: Some("break clause is p.3".into()),
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
    async fn a_journal_cue_at_capture_files_the_note_as_an_entry_and_can_be_undone() {
        let core = test_core().await;
        // The door said so — the cue table retired with the classifier, and
        // an unforced note waits for the judged synthesis call.
        let out = core
            .ingest_capture(
                Capture::new("Dear diary, the move is over.", "ui")
                    .with_intent(Some(crate::core::moments::Intent::Journal)),
            )
            .await
            .unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, "journal");
        assert_eq!(c.metadata["origin_was"], "ui");
        core.set_entry(&out.id, false).await.unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, "ui");
        assert!(c.metadata.get("origin_was").is_none());
        core.set_entry(&out.id, true).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
    }

    /// The undo is idempotent, and a note that is not an entry has none.
    ///
    /// `origin_was` is removed by the first undo, so a second one — the `on=0`
    /// form on the capture receipt re-posted, a back-navigation — found
    /// nothing to restore and fell through to `web`. That rewrote the channel
    /// a capture actually came through, on a corpus the button was never about.
    #[tokio::test]
    async fn undoing_an_entry_twice_leaves_the_channel_it_came_through() {
        let core = test_core().await;
        let out = core
            .ingest_capture(
                Capture::new("Dear diary, the move is over.", "cli")
                    .with_intent(Some(crate::core::moments::Intent::Journal)),
            )
            .await
            .unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
        core.set_entry(&out.id, false).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "cli");
        core.set_entry(&out.id, false).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().origin,
            "cli",
            "a second undo invented a channel the capture never came through"
        );

        // And a note that was never an entry is untouched by the off switch.
        let plain = core.ingest_capture(Capture::new("Die Portnummer ist 8443.", "share")).await.unwrap();
        assert_eq!(core.store.get_corpus(&plain.id).await.unwrap().origin, "share");
        core.set_entry(&plain.id, false).await.unwrap();
        assert_eq!(core.store.get_corpus(&plain.id).await.unwrap().origin, "share");
    }

    #[tokio::test]
    async fn a_journal_cue_through_the_api_or_mcp_is_left_alone() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Heute war ein langer Tag.", "mcp")).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "mcp");
    }

    #[tokio::test]
    async fn a_forced_remind_outranks_the_journal_cue() {
        // `engram -r "Heute den Bericht abgeben"` opens like a diary entry and
        // is a reminder anyway: the door said so.
        let core = test_core().await;
        let out = core
            .ingest_capture(
                Capture::new("Heute den Bericht abgeben.", "ui")
                    .with_intent(Some(crate::core::moments::Intent::Remind)),
            )
            .await
            .unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.origin, "ui", "not filed as an entry");
        assert!(c.metadata.get("origin_was").is_none());
        crate::jobs::test_support::drain(&core).await;
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1, "and it is a reminder");
    }

    #[tokio::test]
    async fn a_forced_journal_is_filed_on_the_doors_word_alone() {
        // `engram -j` on a note that reads like nothing in particular. The
        // caller said what this is; asking the cue table to agree made an
        // explicit instruction conditional on a guess about the text.
        let core = test_core().await;
        let out = core
            .ingest_capture(
                Capture::new("The roof, and then the gutters.", "ui")
                    .with_intent(Some(crate::core::moments::Intent::Journal)),
            )
            .await
            .unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
    }

    #[tokio::test]
    async fn the_zone_a_door_sent_lands_in_metadata() {
        let core = test_core().await;
        let out = core
            .ingest_capture(Capture::new("x", "ui").with_tz(Some("Europe/Berlin".into())))
            .await
            .unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().metadata["tz"], "Europe/Berlin");
    }
}
