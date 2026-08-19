# Embedding Recipe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the embedder asymmetric and templated — queries and documents rendered through EmbeddingGemma's prompt templates before they reach the model, with the envelope's tokens charged against the split budget — so the retrieval baseline moves once, on its own, before the tiered-synthesis modes land on top of it.

**Architecture:** Three template strings live on `EmbedRole` (config) and travel into every `Embedder` as an `EmbedTemplates` value. The `Embedder` trait splits into a wire-level `embed_raw` that implementations provide and two provided methods, `embed_documents` and `embed_query`, that render first — so no call site can forget to render. The embed job measures what it will send through the same `render_document`, which is what makes the splitter and the embedder agree about size.

**Tech Stack:** Rust 2024 (rust-version 1.94), `async_trait`, `serde` + `config` crate for TOML, `wiremock` for HTTP tests, SQLite/Qdrant untouched.

**Spec:** `docs/superpowers/specs/2026-08-19-tiered-synthesis-design.md`, section 2 ("The embedding recipe"). This plan is step 1 of the spec's "Measurement" order; sections 1, 3–9 are later plans and nothing here anticipates them.

## Global Constraints

- Default templates, verbatim from the spec and the model card (2026-08-19):
  `query_template = "task: search result | query: {text}"`,
  `document_template = "title: {title} | text: {text}"`,
  `document_template_untitled = "title: none | text: {text}"`.
- Two document templates, not one plus a filler: the untitled case is a literal substitution on the model card, and `embed_text`'s `Some/None` match already has that shape.
- Templates are config, not code: pointing at `bge-m3` must remain possible. The legacy rendering (`{text}` / `{title}\n{text}` / `{text}`) is what a bge-m3 operator configures and what the test doubles use by default.
- The trait split is not optional (spec: "Rendering at the call site works until someone adds a fourth place that embeds something").
- Envelope cost is charged: `envelope_cost(title) = count(render_document(title, ""))` replaces every `title_cost`, and the existing "a title that fills the limit on its own is refused" behaviour keeps working for an envelope that fills it.
- No fingerprint, no startup guard, no rebuild path, no migration: "There is no legacy base to migrate." Changing a template later means drop the collection and re-capture.
- Existing tests asserting the joined embedding text are updated to the new rendering, not pinned.
- Nothing here changes ranking parameters, `weak_below`, `NEIGHBOUR_*`, or the sparse encoding of queries.
- Commit messages in this repo are one lower-case imperative line, often with a colon prefix (`feat:`, `fix:`, `docs:`, `chore:`); no trailers except the Co-Authored-By/Claude-Session lines the harness requires.
- Run `cargo fmt` before every commit and `cargo clippy --all-targets` at the end of every task; both must be clean.

---

## File structure

| file | responsibility after this plan |
|---|---|
| `src/config.rs` | `EmbedTemplates` (the three strings, defaults, `legacy()`, `render_query`, `render_document`); three new fields on `EmbedRole` with serde defaults and `EmbedRole::templates()`; validation that every template has its placeholders |
| `src/infer/mod.rs` | `EmbedDoc`; the split `Embedder` trait — required `embed_raw`/`templates`/`dim`/`model`/`max_input_tokens`, provided `embed_documents`/`embed_query`/`render_document` |
| `src/infer/openai.rs` | `HttpEmbedder` holds `EmbedTemplates` from config; implements `embed_raw` (the POST, unchanged) and `templates` |
| `src/infer/fake.rs` | `FakeEmbedder` holds templates (legacy by default, `with_templates` for the asymmetric case); `StrictEmbedder` delegates |
| `src/core/search.rs` | the one query site calls `embed_query`; the `BlockingEmbedder` test double adapts |
| `src/core/ask/mod.rs` | the `Keyed` test double adapts |
| `src/jobs/embed.rs` | every document embedding goes through `embed_documents`; `render(core, chunk)` for budgets; `lexical_text` for the sparse half; `envelope_cost` replaces `title_cost` in `split_oversize` and `embed_head`; the "embed as-is" path stops dropping the title |
| `src/main.rs` | the `EmbedRole` literal gains the three fields |
| `config.example.toml`, `README.md` | EmbeddingGemma is the documented default; the bge-m3 block shows the legacy templates |

---

### Task 1: `EmbedTemplates` — the three strings and how they render

**Files:**
- Modify: `src/config.rs` (add after `EmbedRole`, currently at `src/config.rs:651-661`)
- Test: `src/config.rs` (the existing `#[cfg(test)] mod tests` at the bottom of the file)

**Interfaces:**
- Consumes: nothing new.
- Produces (later tasks rely on these exact names):
  ```rust
  pub struct EmbedTemplates { pub query_template: String, pub document_template: String, pub document_template_untitled: String }
  impl Default for EmbedTemplates            // EmbeddingGemma strings
  impl EmbedTemplates {
      pub fn legacy() -> Self;               // "{text}", "{title}\n{text}", "{text}"
      pub fn render_query(&self, query: &str) -> String;
      pub fn render_document(&self, title: Option<&str>, text: &str) -> String;
      pub fn validate(&self) -> Result<(), String>;   // every template has "{text}"; document_template has "{title}"
  }
  pub(crate) fn substitute(template: &str, vars: &[(&str, &str)]) -> String;  // single left-to-right pass
  ```

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` at the bottom of `src/config.rs` (after the last existing test):

```rust
    #[test]
    fn query_and_document_render_differently_for_the_same_text() {
        let t = EmbedTemplates::default();
        let q = t.render_query("how do I recover deleted entries");
        let d = t.render_document(None, "how do I recover deleted entries");
        assert_ne!(q, d);
        assert_eq!(q, "task: search result | query: how do I recover deleted entries");
        assert_eq!(d, "title: none | text: how do I recover deleted entries");
    }

    #[test]
    fn a_titled_and_an_untitled_document_take_different_templates() {
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("Recovering deleted entries"), "run fsck first"),
            "title: Recovering deleted entries | text: run fsck first"
        );
        assert_eq!(
            t.render_document(None, "run fsck first"),
            "title: none | text: run fsck first"
        );
    }

    #[test]
    fn the_legacy_templates_reproduce_the_old_join() {
        // What `embed_text` produced before templates existed, and what a
        // bge-m3 operator configures. The test doubles default to it so every
        // retrieval test that queries with "title\ntext" keeps matching.
        let t = EmbedTemplates::legacy();
        assert_eq!(t.render_query("t0\nalpha"), "t0\nalpha");
        assert_eq!(t.render_document(Some("t0"), "alpha"), "t0\nalpha");
        assert_eq!(t.render_document(None, "alpha"), "alpha");
    }

    #[test]
    fn substitution_is_one_pass_so_a_value_cannot_be_substituted_again() {
        // A title that happens to contain the text placeholder must not have
        // the body spliced into it. Two chained `replace` calls would.
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("about {text}"), "body"),
            "title: about {text} | text: body"
        );
        assert_eq!(
            substitute("a {x} b {y} c {x}", &[("x", "1"), ("y", "2")]),
            "a 1 b 2 c 1"
        );
        // An unknown placeholder is left as written rather than eaten.
        assert_eq!(substitute("{nope} {x}", &[("x", "1")]), "{nope} 1");
    }

    #[test]
    fn a_template_without_its_placeholder_is_rejected() {
        let mut t = EmbedTemplates::default();
        assert!(t.validate().is_ok());
        t.query_template = "task: search result | query: ".into();
        assert!(t.validate().unwrap_err().contains("query_template"));
        let mut t = EmbedTemplates::default();
        t.document_template = "text: {text}".into();
        assert!(t.validate().unwrap_err().contains("{title}"));
        let mut t = EmbedTemplates::default();
        t.document_template_untitled = "title: none | text: ".into();
        assert!(t.validate().unwrap_err().contains("document_template_untitled"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::query_and_document_render_differently_for_the_same_text config::tests::a_titled_and_an_untitled_document_take_different_templates config::tests::the_legacy_templates_reproduce_the_old_join config::tests::substitution_is_one_pass config::tests::a_template_without_its_placeholder_is_rejected 2>&1 | tail -20`
Expected: compile error — `EmbedTemplates` and `substitute` not found.

- [ ] **Step 3: Implement `EmbedTemplates`**

Insert into `src/config.rs` immediately after the `EmbedRole` struct (after line 661, before `pub struct AskRole`):

```rust
/// The three strings that, with `model`, fix what a stored vector means.
///
/// EmbeddingGemma is asymmetric: a query and a document are embedded through
/// different prompts, and a document without a title substitutes the literal
/// `none` rather than an empty field. Changing any of these invalidates every
/// stored vector as thoroughly as changing `model` does; there is no fingerprint
/// or rebuild path because there is no base to look after yet — the answer is
/// to drop the collection and re-capture.
///
/// Kept in config rather than code so that a symmetric embedder — `bge-m3`,
/// which engram shipped with — stays one TOML block away: see `legacy()`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedTemplates {
    /// `{text}` is the query.
    pub query_template: String,
    /// `{title}` and `{text}`.
    pub document_template: String,
    /// `{text}` only; used when the document has no title.
    pub document_template_untitled: String,
}

pub(crate) fn default_query_template() -> String {
    "task: search result | query: {text}".into()
}
pub(crate) fn default_document_template() -> String {
    "title: {title} | text: {text}".into()
}
pub(crate) fn default_document_template_untitled() -> String {
    "title: none | text: {text}".into()
}

impl Default for EmbedTemplates {
    fn default() -> Self {
        Self {
            query_template: default_query_template(),
            document_template: default_document_template(),
            document_template_untitled: default_document_template_untitled(),
        }
    }
}

impl EmbedTemplates {
    /// What `embed_text` produced before templates existed: the title on its
    /// own line above the body, the query as typed. The right block for a
    /// symmetric embedder, and what every test double renders with, so a test
    /// that queries with `"title\ntext"` lands on the document it seeded.
    pub fn legacy() -> Self {
        Self {
            query_template: "{text}".into(),
            document_template: "{title}\n{text}".into(),
            document_template_untitled: "{text}".into(),
        }
    }

    pub fn render_query(&self, query: &str) -> String {
        substitute(&self.query_template, &[("text", query)])
    }

    pub fn render_document(&self, title: Option<&str>, text: &str) -> String {
        match title {
            Some(t) => substitute(&self.document_template, &[("title", t), ("text", text)]),
            None => substitute(&self.document_template_untitled, &[("text", text)]),
        }
    }

    /// Every template must be able to carry what it is for. Checked at config
    /// load; a template that drops `{text}` would embed the same string for
    /// every document and nothing downstream would notice.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.query_template.contains("{text}") {
            return Err("infer.embed.query_template must contain {text}".into());
        }
        if !self.document_template.contains("{text}") {
            return Err("infer.embed.document_template must contain {text}".into());
        }
        if !self.document_template.contains("{title}") {
            return Err("infer.embed.document_template must contain {title}; \
                        use document_template_untitled for the case with no title"
                .into());
        }
        if !self.document_template_untitled.contains("{text}") {
            return Err("infer.embed.document_template_untitled must contain {text}".into());
        }
        Ok(())
    }
}

/// Fill `{name}` placeholders in one left-to-right pass.
///
/// One pass rather than chained `replace` calls: a value that itself contains
/// a placeholder — a heading that reads "about {text}" — must land verbatim,
/// not have the next value spliced into it. A `{name}` that matches no
/// variable is left as written.
pub(crate) fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config::tests:: 2>&1 | tail -20`
Expected: all config tests PASS, including the five new ones.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/config.rs
git commit -m "feat: embed templates — the three strings that fix what a vector means"
```

---

### Task 2: Templates on `EmbedRole`, with defaults and validation

**Files:**
- Modify: `src/config.rs:651-661` (`EmbedRole`), `src/config.rs:1079-1089` (`validate`)
- Modify: `src/main.rs:325-332` (the `EmbedRole` literal)
- Modify: `src/infer/openai.rs:1419-1428` (`embed_cfg` test helper)
- Test: `src/config.rs` `mod tests`

**Interfaces:**
- Consumes: `EmbedTemplates`, `default_*_template()` from Task 1.
- Produces:
  ```rust
  pub struct EmbedRole { /* existing fields */ pub query_template: String, pub document_template: String, pub document_template_untitled: String }
  impl EmbedRole { pub fn templates(&self) -> EmbedTemplates; }
  ```

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/config.rs`. The helper `load_infer` and the `env_guard()` pattern already exist there (`src/config.rs:1646`); copy the `[server]/[store]/[vector]/[infer.tiers...]` preamble from `a_role_resolves_its_endpoint_from_its_tier` exactly as that test writes it, then add the body below. Read that test first and reuse its preamble verbatim — the `[infer.synthesize]` and `[infer.ask]` blocks it contains are required for the config to load at all.

```rust
    /// The preamble every `[infer.embed]` test below shares: a valid config
    /// with the embed block left for the test to write.
    const EMBED_PREAMBLE: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.ask]
        tier = "efficient"
    "#;

    #[test]
    fn embed_templates_default_to_embeddinggemma_when_unset() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::default());
    }

    #[test]
    fn embed_templates_are_read_from_config() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"bge-m3\"
            dim = 1024
            max_input_tokens = 1024
            query_template = \"{{text}}\"
            document_template = \"{{title}}\\n{{text}}\"
            document_template_untitled = \"{{text}}\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::legacy());
    }

    #[test]
    fn a_template_missing_its_placeholder_fails_at_load() {
        let _guard = env_guard();
        let err = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            query_template = \"task: search result | query: \"
            "
        ))
        .unwrap_err();
        assert!(err.to_string().contains("query_template"), "got: {err}");
    }
```

Note the doubled braces inside `format!` — `{{text}}` renders as `{text}` in the TOML. `EMBED_PREAMBLE` is the preamble of `a_role_resolves_its_endpoint_from_its_tier` (`src/config.rs:1655`) minus its `[infer.embed]` block; `output_ratio` is a required key on `[infer.synthesize]`, which is why it is there. The existing `[infer.embed]` blocks elsewhere in these tests (`model = "bge-m3"`, no template keys) keep loading — the keys default.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::embed_templates 2>&1 | tail -20`
Expected: compile error — no method `templates` on `EmbedRole` (and the third test fails because the load succeeds).

- [ ] **Step 3: Add the fields, `templates()`, and validation**

Replace `EmbedRole` in `src/config.rs:651-661` with:

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct EmbedRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub dim: usize,
    pub max_input_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `EmbedTemplates`. Flat on the role rather than nested, so the TOML
    /// reads `[infer.embed] query_template = ...` beside `model`, which is the
    /// other half of the same identity.
    #[serde(default = "default_query_template")]
    pub query_template: String,
    #[serde(default = "default_document_template")]
    pub document_template: String,
    #[serde(default = "default_document_template_untitled")]
    pub document_template_untitled: String,
}

impl EmbedRole {
    pub fn templates(&self) -> EmbedTemplates {
        EmbedTemplates {
            query_template: self.query_template.clone(),
            document_template: self.document_template.clone(),
            document_template_untitled: self.document_template_untitled.clone(),
        }
    }
}
```

In `validate()` (`src/config.rs:1079`), add before the final `Ok(())`:

```rust
        self.infer
            .embed
            .templates()
            .validate()
            .map_err(ConfigError::Invalid)?;
```

Update the literal in `src/main.rs:325-332`:

```rust
                embed: EmbedRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "e".into(),
                    api_key: None,
                    dim: 1024,
                    max_input_tokens: 8192,
                    timeout_secs: engram::config::DEFAULT_TIMEOUT_SECS,
                    query_template: engram::config::EmbedTemplates::default().query_template,
                    document_template: engram::config::EmbedTemplates::default().document_template,
                    document_template_untitled: engram::config::EmbedTemplates::default()
                        .document_template_untitled,
                },
```

Update `embed_cfg` in `src/infer/openai.rs:1419-1428`:

```rust
    fn embed_cfg(base: String) -> EmbedRole {
        let t = crate::config::EmbedTemplates::default();
        EmbedRole {
            base_url: base,
            model: "e".into(),
            api_key: None,
            dim: 4,
            max_input_tokens: 512,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            query_template: t.query_template,
            document_template: t.document_template,
            document_template_untitled: t.document_template_untitled,
        }
    }
```

If `grep -rn "EmbedRole {" src tests` shows any other literal, give it the same three fields.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config::tests:: 2>&1 | tail -20 && cargo build --all-targets 2>&1 | tail -5`
Expected: config tests PASS; the whole crate (including tests and `main.rs`) builds.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/config.rs src/main.rs src/infer/openai.rs
git commit -m "feat: embed templates are config, beside the model they belong to"
```

---

### Task 3: Split the `Embedder` trait and move every caller onto it

This is one task because the trait change forces every implementation and every call site to move together; the crate does not compile in between. Behaviour is preserved for the test doubles (legacy templates) so the existing suite is the regression net.

**Files:**
- Modify: `src/infer/mod.rs:65-71` (`Embedder` trait)
- Modify: `src/infer/openai.rs:618-700` (`HttpEmbedder`)
- Modify: `src/infer/fake.rs:25-91` (`FakeEmbedder`), `src/infer/fake.rs:348-420` (`StrictEmbedder`)
- Modify: `src/core/search.rs:700-708` (query embedding), `src/core/search.rs:983-1008` (`BlockingEmbedder`)
- Modify: `src/core/ask/mod.rs:987-1012` (`Keyed`)
- Modify: `src/jobs/embed.rs` — `embed_text` (line 18), `run_with_limit` (69), `run_corpus_with_limit` (130), `embed_batch` (239-273), `split_oversize` as-is path (402-403), `embed_head` (484-490), and the tests at 890, 921, 957, 1258, 1702
- Test: `src/infer/openai.rs` `mod tests`, `src/infer/fake.rs` (new `mod tests`), `src/jobs/embed.rs` `mod tests`

**Interfaces:**
- Consumes: `EmbedTemplates` (Task 1), `EmbedRole::templates()` (Task 2).
- Produces:
  ```rust
  // src/infer/mod.rs
  pub struct EmbedDoc { pub title: Option<String>, pub text: String }
  #[async_trait] pub trait Embedder: Send + Sync {
      async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;   // required: the wire call
      fn templates(&self) -> &EmbedTemplates;                                // required
      fn dim(&self) -> usize; fn model(&self) -> &str; fn max_input_tokens(&self) -> usize;
      fn render_document(&self, doc: &EmbedDoc) -> String;                   // provided
      async fn embed_documents(&self, docs: &[EmbedDoc]) -> Result<Vec<Vec<f32>>>; // provided
      async fn embed_query(&self, query: &str) -> Result<Vec<f32>>;          // provided
  }
  // src/infer/fake.rs
  impl FakeEmbedder { pub fn new(dim) -> Self /* legacy templates */; pub fn with_templates(dim, EmbedTemplates) -> Self; pub fn calls(&self) -> usize; pub fn sent(&self) -> Vec<String> /* every rendered string embed_raw received, in order */ }
  // src/jobs/embed.rs (private)
  fn doc_of(chunk: &Chunk) -> EmbedDoc; fn render(core: &Core, chunk: &Chunk) -> String; fn lexical_text(chunk: &Chunk) -> String;
  async fn embed_batch(core: &Core, chunks: &[Chunk]) -> Result<()>;         // signature changes: no texts argument
  ```

- [ ] **Step 1: Write the failing tests**

In `src/infer/openai.rs` `mod tests`, after `embedder_sends_float_encoding_and_orders_results_by_index`:

```rust
    #[tokio::test]
    async fn documents_and_queries_are_rendered_through_the_templates_before_the_post() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let one = serde_json::json!({"data":[{"index":0,"embedding":[1.0,0.0,0.0,0.0]}]});
        let two = serde_json::json!({"data":[
            {"index":0,"embedding":[1.0,0.0,0.0,0.0]},
            {"index":1,"embedding":[0.0,1.0,0.0,0.0]}
        ]});
        // The document side: titled and untitled take different templates.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "input": ["title: Recovering | text: run fsck", "title: none | text: bare"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(two))
            .expect(1)
            .mount(&server)
            .await;
        // The query side: the retrieval task prefix, nothing else.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "input": ["task: search result | query: fsck"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(one))
            .expect(1)
            .mount(&server)
            .await;

        let e = HttpEmbedder::new(&embed_cfg(server.uri()));
        let docs = vec![
            crate::infer::EmbedDoc { title: Some("Recovering".into()), text: "run fsck".into() },
            crate::infer::EmbedDoc { title: None, text: "bare".into() },
        ];
        let out = e.embed_documents(&docs).await.unwrap();
        assert_eq!(out.len(), 2);
        let q = e.embed_query("fsck").await.unwrap();
        assert_eq!(q, vec![1.0, 0.0, 0.0, 0.0]);
        // `.expect(1)` on both mocks is verified when `server` drops.
    }
```

In `src/infer/fake.rs`, add at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmbedTemplates;
    use crate::infer::EmbedDoc;

    #[tokio::test]
    async fn the_fake_renders_like_the_real_one_and_keeps_what_it_sent() {
        // With the asymmetric templates the same words embed to different
        // vectors as a query and as a document — the property the real
        // embedder has, exercised here so a test can rely on it.
        let e = FakeEmbedder::with_templates(8, EmbedTemplates::default());
        let d = e
            .embed_documents(&[EmbedDoc { title: None, text: "alpha".into() }])
            .await
            .unwrap();
        let q = e.embed_query("alpha").await.unwrap();
        assert_ne!(d[0], q);
        assert_eq!(
            e.sent(),
            vec![
                "title: none | text: alpha".to_string(),
                "task: search result | query: alpha".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn the_default_fake_is_symmetric_so_a_query_can_name_a_document() {
        // What every retrieval test in the crate depends on: querying with
        // "title\ntext" lands on the document seeded with that title and text.
        let e = FakeEmbedder::new(8);
        let d = e
            .embed_documents(&[EmbedDoc { title: Some("t0".into()), text: "alpha".into() }])
            .await
            .unwrap();
        let q = e.embed_query("t0\nalpha").await.unwrap();
        assert_eq!(d[0], q);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib infer:: 2>&1 | tail -20`
Expected: compile errors — `EmbedDoc`, `embed_documents`, `embed_query`, `with_templates`, `sent` do not exist.

- [ ] **Step 3: Rewrite the trait**

Replace the `Embedder` trait in `src/infer/mod.rs:65-71` with:

```rust
/// One document as the embedder sees it: what goes into the `title:` slot and
/// what goes into the `text:` slot. Built by `jobs::embed` from a `Chunk`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedDoc {
    pub title: Option<String>,
    pub text: String,
}

/// Asymmetric by interface. A query and a document are rendered through
/// different templates before they reach the model, and the rendering happens
/// *inside* the trait — `embed_documents` and `embed_query` are provided — so
/// there is no call site that can forget it. `embed_raw` is the wire call and
/// is what an implementation supplies; nothing outside an `Embedder` should
/// call it.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// The strings sent, exactly as given. Implementations only.
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn templates(&self) -> &crate::config::EmbedTemplates;
    fn dim(&self) -> usize;
    fn model(&self) -> &str;
    fn max_input_tokens(&self) -> usize;

    /// The string a document becomes. Exposed so budgets can be measured
    /// against what will actually be sent — the envelope costs tokens too.
    fn render_document(&self, doc: &EmbedDoc) -> String {
        self.templates()
            .render_document(doc.title.as_deref(), &doc.text)
    }

    async fn embed_documents(&self, docs: &[EmbedDoc]) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = docs.iter().map(|d| self.render_document(d)).collect();
        self.embed_raw(&texts).await
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut out = self
            .embed_raw(&[self.templates().render_query(query)])
            .await?;
        if out.is_empty() {
            return Err(crate::error::Error::Inference {
                role: "embed",
                detail: "no vector came back for the query".into(),
            });
        }
        Ok(out.remove(0))
    }
}
```

- [ ] **Step 4: Move `HttpEmbedder`**

In `src/infer/openai.rs:618-641`, add the field and rename the method:

```rust
pub struct HttpEmbedder {
    ep: Endpoint,
    dim: usize,
    max_input_tokens: usize,
    templates: crate::config::EmbedTemplates,
}

impl HttpEmbedder {
    pub fn new(cfg: &EmbedRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "embed",
            ),
            dim: cfg.dim,
            max_input_tokens: cfg.max_input_tokens,
            templates: cfg.templates(),
        }
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // body unchanged from the old `embed`
```

Keep the body of the old `embed` exactly as it is under the new name. Below the existing `fn max_input_tokens`, add:

```rust
    fn templates(&self) -> &crate::config::EmbedTemplates {
        &self.templates
    }
```

Then fix the existing `HttpEmbedder` tests in this file that call `.embed(&[...])` (lines ~1729, 1756, 1773, 1791, 2312): they exercise the wire path — retry classification, index ordering, dimension check, short batch — so change each `.embed(` to `.embed_raw(` and leave their inputs as they are.

- [ ] **Step 5: Move the fakes**

Replace `FakeEmbedder` in `src/infer/fake.rs:25-91` with:

```rust
/// Hashes text into a fixed-dimension unit vector. Identical text gives an
/// identical vector and different text gives a different one, which is all the
/// retrieval tests need from an embedding model.
///
/// Renders with the *legacy* templates by default — `{text}` for a query,
/// `{title}\n{text}` for a document — so a test that queries with
/// `"title\ntext"` lands on the document it seeded, exactly as before
/// templates existed. `with_templates` gives a fake the asymmetric recipe for
/// the tests that are about the recipe.
pub struct FakeEmbedder {
    dim: usize,
    templates: crate::config::EmbedTemplates,
    /// How many times the endpoint was called. Batching is invisible in the
    /// output — only the call count shows whether it happened.
    calls: std::sync::atomic::AtomicUsize,
    /// Every string handed to `embed_raw`, in order: what a real endpoint
    /// would have been sent.
    sent: std::sync::Mutex<Vec<String>>,
    /// When set, every call is refused with this reason — the endpoint's "no",
    /// which a worker must not retry.
    reject_with: Option<String>,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self::with_templates(dim, crate::config::EmbedTemplates::legacy())
    }

    pub fn with_templates(dim: usize, templates: crate::config::EmbedTemplates) -> Self {
        Self {
            dim,
            templates,
            calls: std::sync::atomic::AtomicUsize::new(0),
            sent: std::sync::Mutex::new(Vec::new()),
            reject_with: None,
        }
    }

    pub fn rejecting(msg: &str) -> Self {
        let mut e = Self::new(8);
        e.reject_with = Some(msg.to_string());
        e
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What was sent, rendered, in order.
    pub fn sent(&self) -> Vec<String> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut s) = self.sent.lock() {
            s.extend(texts.iter().cloned());
        }
        if let Some(m) = &self.reject_with {
            return Err(Error::InferenceRejected {
                role: "embed",
                detail: m.clone(),
            });
        }
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.dim];
                let mut seed = Sha256::digest(t.as_bytes()).to_vec();
                for i in 0..self.dim {
                    if i % 32 == 0 && i > 0 {
                        seed = Sha256::digest(&seed).to_vec();
                    }
                    v[i] = (seed[i % 32] as f32 - 128.0) / 128.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
    fn templates(&self) -> &crate::config::EmbedTemplates {
        &self.templates
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn model(&self) -> &str {
        "fake-embed"
    }
    fn max_input_tokens(&self) -> usize {
        8192
    }
}
```

In `StrictEmbedder` (`src/infer/fake.rs:377-420`): rename `async fn embed` to `async fn embed_raw`, change its final line `self.inner.embed(texts).await` to `self.inner.embed_raw(texts).await`, and add:

```rust
    fn templates(&self) -> &crate::config::EmbedTemplates {
        self.inner.templates()
    }
```

The length check inside it (`t.len() / 4`) now measures the rendered string, which is what a real server would measure — no change needed.

- [ ] **Step 6: Move the two in-test doubles**

`src/core/search.rs:992-1008` (`BlockingEmbedder`): rename `embed` → `embed_raw`, its body's `self.inner.embed(texts)` → `self.inner.embed_raw(texts)`, and add `fn templates(&self) -> &crate::config::EmbedTemplates { self.inner.templates() }`.

`src/core/ask/mod.rs:990-1012` (`Keyed`): rename `embed` → `embed_raw`; it has no inner, so add:

```rust
        fn templates(&self) -> &crate::config::EmbedTemplates {
            static LEGACY: std::sync::LazyLock<crate::config::EmbedTemplates> =
                std::sync::LazyLock::new(crate::config::EmbedTemplates::legacy);
            &LEGACY
        }
```

Its `contains("alpha")` checks still see the word inside the rendered string.

- [ ] **Step 7: Move the query site**

`src/core/search.rs:701-706`: replace

```rust
                let v = self
                    .embedder
                    .embed(&[query.q.trim().to_string()])
                    .await?
                    .remove(0);
```

with

```rust
                let v = self.embedder.embed_query(query.q.trim()).await?;
```

- [ ] **Step 8: Move the embed job onto documents**

In `src/jobs/embed.rs`, add `use crate::infer::EmbedDoc;` to the imports at the top of the file, then replace `embed_text` (lines 16-23) with three helpers:

```rust
/// The document as the embedder will see it.
fn doc_of(chunk: &Chunk) -> EmbedDoc {
    EmbedDoc {
        title: chunk.title.clone(),
        text: chunk.text.clone(),
    }
}

/// What will actually be sent for this chunk — title slot, text slot and the
/// template around them. Every budget in this file measures this string, so
/// the splitter and the embedder cannot disagree about size.
fn render(core: &Core, chunk: &Chunk) -> String {
    core.embedder.render_document(&doc_of(chunk))
}

/// The lexical half. Title on its own line above the body — the words, not the
/// template: `title:` and `text:` are in every document and would match every
/// query that happens to contain them.
fn lexical_text(chunk: &Chunk) -> String {
    match &chunk.title {
        Some(t) => format!("{t}\n{}", chunk.text),
        None => chunk.text.clone(),
    }
}
```

`run_with_limit` (line 67 on): `let text = embed_text(&chunk);` → `let text = render(core, &chunk);`, and the call `embed_batch(core, std::slice::from_ref(&chunk), vec![text.clone()])` → `embed_batch(core, std::slice::from_ref(&chunk))`. The `measured` line below it keeps using `text`.

`run_corpus_with_limit` (line 120 on): `let text = embed_text(&chunk);` → `let text = render(core, &chunk);`; delete the `texts` vector entirely (its two `push`/`with_capacity` lines and the `texts[..take].to_vec()` argument), so the call reads `embed_batch(core, &batch[..take])`. The `if core.counter.count(&text) > limit` test stays.

`embed_batch` (line 239): new signature and body head:

```rust
async fn embed_batch(core: &Core, chunks: &[Chunk]) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let docs: Vec<EmbedDoc> = chunks.iter().map(doc_of).collect();
    let vectors = core.embedder.embed_documents(&docs).await?;
    if vectors.len() != chunks.len() {
        // unchanged error
    }

    let points = chunks
        .iter()
        .zip(vectors)
        .map(|(c, vector)| VectorPoint {
            vector,
            // The words the dense side saw, without the template around them,
            // so the lexical and the semantic half of a hit describe the same
            // document.
            sparse: crate::vector::sparse::encode_document(&lexical_text(c)),
            payload: payload_of(c),
        })
        .collect();
```

`split_oversize`, the as-is path (lines 402-404): `core.embedder.embed(std::slice::from_ref(&chunk.text))` → `core.embedder.embed_documents(std::slice::from_ref(&doc_of(chunk)))`. This stops that one path from dropping the title — the limit was checked against title + text and the vector was made from text alone; now both see the same document. Its `sparse:` line below becomes `encode_document(&lexical_text(chunk))`.

`embed_head` (line 484-490): replace

```rust
    let input = match chunk.title.as_deref() {
        Some(t) => format!("{t}\n{head}"),
        None => head,
    };
    let permit = core.gate.background().await;
    let embedded = core.embedder.embed(std::slice::from_ref(&input)).await;
```

with

```rust
    let input = EmbedDoc {
        title: chunk.title.clone(),
        text: head,
    };
    let permit = core.gate.background().await;
    let embedded = core
        .embedder
        .embed_documents(std::slice::from_ref(&input))
        .await;
```

and its `sparse:` line becomes `encode_document(&lexical_text(chunk))` (it already encodes the whole artifact; the helper is the same join).

Leave `title_cost` in `split_oversize` and `embed_head` alone in this task — Task 4 changes it.

- [ ] **Step 9: Update the tests in `embed.rs` that built embed text by hand**

- Lines ~890, 921, 957: `embed_batch(&core, std::slice::from_ref(&stale), vec![embed_text(&stale)])` → `embed_batch(&core, std::slice::from_ref(&stale))` (and `&hidden` likewise).
- Line ~1258: `core.counter.count(&embed_text(c)) <= limit` → `core.counter.count(&render(&core, c)) <= limit`.
- Line ~1702: replace

```rust
        let q = core
            .embedder
            .embed(&["t0\n## A\nthe body".to_string()])
            .await
            .unwrap();
        let hits = core
            .vectors
            .search(&q[0], &Default::default(), 5, &Default::default())
```

with

```rust
        let q = core
            .embedder
            .embed_query("t0\n## A\nthe body")
            .await
            .unwrap();
        let hits = core
            .vectors
            .search(&q, &Default::default(), 5, &Default::default())
```

Then `grep -rn "\.embed(" src tests` — every remaining hit must be inside an `impl Embedder` (none should remain) or be a non-embedder method. Fix any stragglers the same way: documents → `embed_documents(&[EmbedDoc{..}])`, queries → `embed_query(..)`.

- [ ] **Step 10: Build, run the whole suite**

Run: `cargo build --all-targets 2>&1 | grep -E "^(error|warning)" | head` → nothing.
Run: `cargo test 2>&1 | tail -30`
Expected: everything PASSES, including the three new tests. If a retrieval test fails with a hit not found: it queried with a string that is not `"title\ntext"` of what it seeded — check whether it used to pass only because both sides were embedded identically and now the fake renders the document as `title\ntext` while the query is raw; the fix is the query string in the test, never the templates.

- [ ] **Step 11: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head
git add src/infer/mod.rs src/infer/openai.rs src/infer/fake.rs src/core/search.rs src/core/ask/mod.rs src/jobs/embed.rs
git commit -m "feat: the embedder is asymmetric by interface — documents and queries render before the call"
```

---

### Task 4: Charge the envelope against the split budget

**Files:**
- Modify: `src/jobs/embed.rs` — `split_oversize` (`title_cost` at ~line 368), `embed_head` (`title_cost` at ~line 469)
- Test: `src/jobs/embed.rs` `mod tests`

**Interfaces:**
- Consumes: `render`, `doc_of` from Task 3; `FakeEmbedder::with_templates`, `EmbedTemplates::default()`.
- Produces: `fn envelope_cost(core: &Core, title: Option<&str>) -> usize` (private to `embed.rs`).

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/jobs/embed.rs`, directly after `a_chunk_only_its_title_pushes_over_the_limit_does_not_respawn_itself`:

```rust
    #[tokio::test]
    async fn the_envelope_is_charged_so_a_chunk_that_fits_bare_and_overflows_rendered_splits_once() {
        // Same loop as the test above, reopened slightly narrower: with a real
        // template the title is not the only thing around the text. `title: `
        // plus ` | text: ` costs tokens, and a split that budgets for the title
        // alone emits siblings that measure oversize again once rendered —
        // each replaced by another exactly like it, forever.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::FakeEmbedder::with_templates(
            crate::core::test_support::TEST_DIM,
            crate::config::EmbedTemplates::default(),
        ));
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();

        let title = "heading".to_string();
        let text = format!("{}\n\n{}", "alpha ".repeat(12), "beta ".repeat(12));
        let limit = 40;
        let bare = format!("{title}\n{text}");
        assert!(
            core.counter.count(&bare) <= limit,
            "title + text must fit without the envelope ({})",
            core.counter.count(&bare)
        );
        let rendered = core
            .embedder
            .render_document(&crate::infer::EmbedDoc {
                title: Some(title.clone()),
                text: text.clone(),
            });
        assert!(
            core.counter.count(&rendered) > limit,
            "the envelope must be what pushes it over ({})",
            core.counter.count(&rendered)
        );

        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: text.clone(),
                    corpus_span: None,
                    title: Some(title),
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, limit).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "the chunk was not split: {} row(s)", chunks.len());
        assert!(
            !chunks.iter().any(|c| c.text == text && c.id != made[0].id),
            "the parent was replaced by an identical copy of itself"
        );
        for c in &chunks {
            assert!(
                core.counter.count(&render(&core, c)) <= limit,
                "sibling is still oversize once rendered: {:?}",
                c.text
            );
        }
    }
```

If the two `assert!`s on the fixture fail, adjust `repeat(12)` (up or down by one or two) until `bare` fits in 40 and `rendered` does not — the estimator is `chars * 2 / 7`, the envelope is `title:  | text: ` (16 characters, ~4 tokens), so the window is narrow but real.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib jobs::embed::tests::the_envelope_is_charged 2>&1 | tail -20`
Expected: FAIL on "sibling is still oversize once rendered" (siblings were budgeted against the title alone and measure over the limit rendered), or the `run_with_limit` loops until a job-level guard stops it — either way, not PASS.

- [ ] **Step 3: Replace `title_cost` with `envelope_cost`**

Add next to `render` in `src/jobs/embed.rs`:

```rust
/// What the envelope around an empty body costs: the title, and the template
/// around it. Siblings inherit both, so only what this leaves over is available
/// to their text.
fn envelope_cost(core: &Core, title: Option<&str>) -> usize {
    core.counter.count(&core.embedder.render_document(&EmbedDoc {
        title: title.map(str::to_string),
        text: String::new(),
    }))
}
```

In `split_oversize`, replace

```rust
    let title_cost = chunk
        .title
        .as_deref()
        .map_or(0, |t| core.counter.count(&format!("{t}\n")));
    let budget = limit.saturating_sub(title_cost);
```

with

```rust
    let title_cost = envelope_cost(core, chunk.title.as_deref());
    let budget = limit.saturating_sub(title_cost);
```

and update the comment above it: "The limit is checked against what actually gets embedded, and that is the rendered document — title, text and the template around them. Siblings inherit the title and the template, so only what the envelope leaves over is available to their text."

In `embed_head`, make the same replacement of the `title_cost` computation.

Note on the untitled case: `envelope_cost(core, None)` is now non-zero under the default templates (`title: none | text: ` costs a few tokens) where the old `title_cost` was zero. That is correct — the envelope is sent — and it is why the test doubles default to legacy templates: under those, `envelope_cost(None)` is `0` and `envelope_cost(Some(t))` is `count("t\n")`, exactly the old numbers, so every existing split test keeps its arithmetic.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib jobs::embed:: 2>&1 | tail -20`
Expected: all embed tests PASS, including the new one and `a_chunk_only_its_title_pushes_over_the_limit_does_not_respawn_itself`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head
git add src/jobs/embed.rs
git commit -m "fix: the split budget charges the envelope, not just the title"
```

---

### Task 5: EmbeddingGemma is the documented default

**Files:**
- Modify: `config.example.toml:170-183` (`[infer.embed]`)
- Modify: `README.md:217` (the `infer.embed.*` row) and the notes at `README.md:237-250`

**Interfaces:** none (docs only).

- [ ] **Step 1: Rewrite the example block**

Replace `config.example.toml` lines 170-183 with:

```toml
[infer.embed]
base_url = "http://localhost:8000/v1"
# EmbeddingGemma. 308M parameters, 768 dimensions, a 2048-token window, and an
# asymmetric interface: queries and documents are embedded through different
# prompts, which the three templates below are. The model card's exact strings
# are the defaults, so nothing below `max_input_tokens` needs setting for it.
model = "embeddinggemma"
# Embedding is fast even locally; a shorter ceiling surfaces a wedged endpoint
# instead of holding a worker for a quarter of an hour.
timeout_secs = 120
# MUST match the vector dimension of the Qdrant collection. Changing this
# against an existing collection is refused at startup. The full Matryoshka
# width: truncating to 512/256/128 buys memory at a recall cost, and recall is
# the point.
dim = 768
# The server's real ceiling, not the model's nominal one. llama.cpp refuses any
# input above its physical batch size with a 500, and no retry can fix that —
# engram splits the artifact instead, but it splits sooner and cheaper if this
# number is honest. 2048 is the model's window; use the server's if lower.
max_input_tokens = 2048
# The recipe. `{text}` is the query or the body; `{title}` is the artifact's
# title. A document with no title takes the `_untitled` template — the model
# card substitutes the literal word `none`, so it is a second template and not
# an empty field. Changing any of these invalidates every stored vector as
# surely as changing `model` does: drop the collection and re-capture. Check
# once that your serving stack does not prepend a prompt of its own, or the
# prefix is sent twice and every vector carries it.
# query_template             = "task: search result | query: {text}"
# document_template          = "title: {title} | text: {text}"
# document_template_untitled = "title: none | text: {text}"
#
# A symmetric embedder — bge-m3, which engram shipped with — wants the text as
# it is. That is these three lines, with `dim = 1024` and the server's ceiling:
# model = "bge-m3"
# dim = 1024
# max_input_tokens = 1024
# query_template             = "{text}"
# document_template          = "{title}\n{text}"
# document_template_untitled = "{text}"
```

- [ ] **Step 2: Update the README**

Replace the table row at `README.md:217`:

```markdown
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`, `timeout_secs`, and the three prompt templates `query_template`, `document_template`, `document_template_untitled`. Defaults are EmbeddingGemma's; a symmetric model sets the three to `{text}` / `{title}\n{text}` / `{text}`. No tier — an embedding endpoint is a different shape of thing, not a cheaper model. |
```

After the existing `infer.embed.max_input_tokens` note (`README.md:244-250`), add:

```markdown
- **`infer.embed.*_template`** and `model` together are one identity: a vector's
  meaning is fixed by the model *and* by the text handed to it. Editing a
  template later silently mixes embedding spaces. There is no rebuild path;
  drop the collection and re-capture.
```

Read the surrounding README section first and match its list style (the existing `- **`infer.embed.dim`**` bullet is the model).

- [ ] **Step 3: Check the example still loads**

Run: `cargo test --lib config::tests:: 2>&1 | tail -5`
Expected: PASS. (If a test loads `config.example.toml` directly — `grep -rn "config.example" src tests` — it must still pass; the active keys above are all ones `EmbedRole` knows.)

- [ ] **Step 4: Commit**

```bash
git add config.example.toml README.md
git commit -m "docs: embeddinggemma is the documented embedder, templates and all"
```

---

### Task 6: Verification, then measure before anything else lands

**Files:** none new.

- [ ] **Step 1: The full gate**

Run, in order, and paste the tails into the task report:

```bash
cargo fmt --check
cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"    # expect 0
cargo test 2>&1 | tail -15                                        # expect "test result: ok" for every binary
cargo test --test eval 2>&1 | tail -5                              # the judge harness still builds and runs
```

- [ ] **Step 2: `grep` the invariants**

```bash
grep -rn "\.embed(" src tests | grep -v "embed_raw\|embed_documents\|embed_query\|fn embed"   # expect nothing
grep -rn "format!(\"{t}\\\\n" src/jobs/embed.rs    # only `lexical_text` may join title and text by hand
grep -n "title_cost" src/jobs/embed.rs             # every hit is assigned from envelope_cost
```

- [ ] **Step 3: Record the measurement step for the operator**

This is not code. The spec's "Measurement" section says the recipe lands on its own and moves the retrieval baseline; the next plan (`off` mode) must not start before one judged-pair run has been recorded against this commit. In the PR description, state: which embedding server and model were used, `dim`/`max_input_tokens`, that the raw request body was checked once for a single prefix (no server-side template), and the `/ui/judge` numbers before and after on the same pair set — or that no pair set exists yet and the baseline is being recorded now.

- [ ] **Step 4: Finish the branch**

Use `superpowers:finishing-a-development-branch`. The branch is `docs/tiered-synthesis` today, which carries the spec; the recipe work belongs on its own branch (`feat/embedding-recipe`) cut from `master` with the spec commit cherry-picked or merged first — check with the operator which, since the spec PR may merge separately.

---

## Self-review

**Spec coverage (§2 only):**
- Three defects → Task 3 (prefixes, asymmetry), Task 3 step 8 (document format).
- "Model plus templates is one identity", no fingerprint/migration → Task 5 docs; nothing built, by design.
- Configuration block (`model`, `dim = 768`, `max_input_tokens = 2048`, three templates) → Task 2 (keys, defaults, validation), Task 5 (example).
- "Two document templates rather than one plus a filler" → Task 1 `render_document`.
- "Templates stay in config… bge-m3 must remain possible" → Task 1 `legacy()`, Task 5 commented block.
- Where it lands table → `EmbedRole` (Task 2), trait (Task 3), `HttpEmbedder`/`FakeEmbedder` (Task 3), `embed_text → render_document` (Task 3 step 8), `search.rs:703` (Task 3 step 7).
- Envelope token accounting → Task 4.
- One known simplification (`gaps.rs` keeps the retrieval template) → no code; `src/core/gaps.rs` clusters stored vectors and calls no embedder, confirmed by `grep`. Stated in the spec; nothing to do.
- Tests listed in the spec: "query and document render differently" (Task 1), "titled and untitled take different templates" (Task 1), "envelope cost is charged … does not respawn itself" (Task 4), "render_document for a passage with a carried heading puts the heading in the title slot" — passages do not exist until the `off`-mode plan; the titled-document test in Task 1 is the same assertion on an artifact's title and the passage variant is owed by that plan.
- "Existing tests asserting the joined embedding text are updated to the new rendering rather than pinned" → Task 3 step 9.

**Placeholder scan:** no TBD/TODO; every code step has code; the one "read the existing test and reuse its preamble" instruction in Task 2 step 1 is accompanied by a full preamble to fall back on.

**Type consistency:** `EmbedTemplates` (Task 1) is what `EmbedRole::templates()` returns (Task 2), what `Embedder::templates()` borrows (Task 3), and what `FakeEmbedder::with_templates` takes (Task 3/4). `EmbedDoc { title: Option<String>, text: String }` is used identically in Tasks 3 and 4. `embed_batch(core, chunks)` has no third argument anywhere after Task 3. `render(core, chunk)` and `envelope_cost(core, title)` are both `embed.rs`-private and named the same in Tasks 3 and 4.

**One deliberate deviation from the spec's wording:** "the fake exercises the asymmetry" — the default `FakeEmbedder` is symmetric (legacy templates) so the ~forty retrieval tests that query with `"title\ntext"` keep passing; the asymmetry is exercised by `FakeEmbedder::with_templates(.., EmbedTemplates::default())` in the tests that are about it (Task 3 fake tests, Task 4). The spec should gain one sentence saying so when the next plan is written.
