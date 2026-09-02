# Small Captures and Workspace UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A one-sentence reminder becomes one artifact in its own language on the right weekday, its verbatim passage is hidden behind it, and the workspace tells a first-time user where to start.

**Architecture:** Part A is four pure-function guards in the Rust job path (artifact allowance, weekday reconciliation, placed-first coverage, text language detection) plus one prompt sentence in ten languages. Part B is CSS and template work with a small amount of app.js: a `html.typing` class that the existing `hideIdle`/`showIdle` pair toggles, a content-shape rule that lends the accent to one verb, and copy changes. No schema changes, no new dependencies.

**Tech Stack:** Rust (axum, askama, chrono, chrono-tz), htmx, vanilla JS, plain CSS. Tests: `cargo test --locked`; lint: `cargo clippy --all-targets --locked -- -D warnings`.

**Spec:** `docs/superpowers/specs/2026-09-02-small-capture-and-ui-findings.md`

## Global Constraints

- Every commit passes `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`.
- Prompt JSON shape, field names, `category` values and the `----- INPUT -----` markers stay byte-identical in every language (`src/infer/lang.rs` module doc).
- The JS bundle is `assets/app.js`, ES5 style (`var`, no arrow functions), no build step.
- CSS files concatenate in numeric order into `assets/app.css`; a rule must live in the file whose number matches its layer (`00-tokens`, `05-background`, `20-layout`, `30-components`, `40-workspace`, `50-phone`).
- Commit messages follow the repo's `type(scope): sentence` style, lowercase, no trailing period.
- Do not touch `src/store/*_schema.sql`.

---

## Part A — the job path

### Task 1: Artifact allowance per window

**Files:**
- Modify: `src/jobs/window.rs:168-186` (after `let mut chunks = reply.artifacts;` and the context-only `retain`)
- Test: `src/jobs/window.rs` (tests module, after `an_artifact_found_only_in_context_is_recognised`)

**Interfaces:**
- Produces: `pub(crate) fn artifact_allowance(input_tokens: usize) -> usize` and `pub(crate) fn within_allowance(chunks: Vec<ProposedArtifact>, window: &str, allowance: usize) -> Vec<ProposedArtifact>`, both in `src/jobs/window.rs`.

- [x] **Step 1: Write the failing tests**

Add to the `tests` module of `src/jobs/window.rs`:

```rust
    #[test]
    fn the_allowance_is_one_artifact_per_thirty_tokens_and_never_zero() {
        assert_eq!(artifact_allowance(0), 1);
        assert_eq!(artifact_allowance(20), 1);
        assert_eq!(artifact_allowance(29), 1);
        assert_eq!(artifact_allowance(70), 2);
        assert_eq!(artifact_allowance(3000), 100);
    }

    #[test]
    fn over_allowance_keeps_located_artifacts_first_then_the_models_order() {
        let window = "erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr.";
        let art = |text: &str| crate::infer::ProposedArtifact {
            text: text.into(),
            title: None,
            category: None,
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
            pinned: false,
        };
        let chunks = vec![
            art("The note references a specific future event on Friday at 13:45."),
            art("erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr."),
            art("The reminder is set for Friday, 2026-09-05 at 13:45."),
        ];
        let kept = within_allowance(chunks, window, 1);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].text.starts_with("erinnere mich"));

        // Under the allowance nothing moves.
        let chunks = vec![art("b"), art("a")];
        let kept = within_allowance(chunks, window, 5);
        assert_eq!(kept.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked --lib jobs::window::tests::the_allowance -- --nocapture`
Expected: compile error, `artifact_allowance` not found.

- [x] **Step 3: Implement the two functions**

Add above `pub(crate) fn from_context_only` in `src/jobs/window.rs`:

```rust
/// The most artifacts a window of `input_tokens` may yield.
///
/// One per thirty tokens, never fewer than one. A 9B model told "if a passage
/// covers three techniques, emit three" and then handed a JUDGE block naming
/// three things wrote a one-sentence reminder up as six artifacts, four of
/// them restating the judgement. The prompt now forbids that; this is the
/// floor under the prompt, because a prompt is a request and a truncation is
/// not. Thirty is generous: a three-line bug report of seventy tokens still
/// gets two, and a chapter gets a hundred.
pub(crate) fn artifact_allowance(input_tokens: usize) -> usize {
    (input_tokens / 30).max(1)
}

/// The artifacts that fit the allowance, located ones first.
///
/// When the model over-delivers, what is kept is what can be traced to the
/// window: an artifact whose text locates verbatim in the source is evidence,
/// a rewrite is a claim. Among equals the model's own order stands, which is
/// what a stable sort gives.
pub(crate) fn within_allowance(
    mut chunks: Vec<crate::infer::ProposedArtifact>,
    window: &str,
    allowance: usize,
) -> Vec<crate::infer::ProposedArtifact> {
    if chunks.len() <= allowance {
        return chunks;
    }
    chunks.sort_by_key(|c| crate::infer::verify::locate_span(&c.text, window, 1).is_none());
    chunks.truncate(allowance);
    chunks
}
```

- [x] **Step 4: Apply the allowance in `run`**

In `src/jobs/window.rs`, directly after the `if !ctx.is_empty() || !neighbor_texts.is_empty() { ... }` block that drops context-only artifacts (it ends with the `tracing::info!(... "artifacts drawn from context blocks were dropped")` call), insert:

```rust
    // The floor under the prompt's "a short note is one artifact". Logged at
    // info like the context drop above: a rising count is the configured
    // model ignoring the prompt, and a number in the journal beats a base
    // full of restated judgements.
    let allowance = artifact_allowance(window_tokens);
    if chunks.len() > allowance {
        tracing::info!(
            corpus_id,
            window = idx,
            proposed = chunks.len(),
            allowance,
            "more artifacts than the window can carry; keeping the located ones"
        );
        chunks = within_allowance(chunks, &text, allowance);
    }
```

`window_tokens` is already in scope (computed for the over-budget check near the top of `run`).

- [x] **Step 5: Run the tests and lint**

Run: `cargo test --locked --lib jobs::window && cargo clippy --all-targets --locked -- -D warnings`
Expected: all window tests pass, clippy clean.

- [x] **Step 6: Commit**

```bash
git add src/jobs/window.rs
git commit -m "fix(window): a window carries one artifact per thirty tokens, located ones first"
```

---

### Task 2: The prompt says the judgement is not an artifact

**Files:**
- Modify: `src/infer/prompt.rs` — the ten `*_SYSTEM` constants (English at `:21-95`, German `:~100-179`, Spanish `:~185-262`, French `:~270-347`, Italian `:~355-431`, Dutch `:~440-514`, Polish `:~520-595`, Portuguese, Russian `:~690-758`, Turkish `:~770-837`)
- Test: `src/infer/prompt.rs` tests module

**Interfaces:**
- Consumes: `pub fn synthesizer_system(lang: Lang) -> &'static str` (exists).

- [x] **Step 1: Write the failing test**

Add to the `tests` module of `src/infer/prompt.rs`:

```rust
    #[test]
    fn every_language_says_the_judgement_is_not_an_artifact() {
        use crate::infer::lang::Lang;
        let phrase = [
            (Lang::En, "The judgement is not an artifact"),
            (Lang::De, "Das Urteil ist kein Artefakt"),
            (Lang::Es, "El juicio no es un artefacto"),
            (Lang::Fr, "Le jugement n'est pas un artefact"),
            (Lang::It, "Il giudizio non è un artefatto"),
            (Lang::Nl, "Het oordeel is geen artefact"),
            (Lang::Pl, "Ocena nie jest artefaktem"),
            (Lang::Pt, "O julgamento não é um artefato"),
            (Lang::Ru, "Суждение — не артефакт"),
            (Lang::Tr, "Yargı bir artefakt değildir"),
        ];
        for (lang, p) in phrase {
            assert!(
                synthesizer_system(lang).contains(p),
                "{lang:?} prompt lacks the one-artifact rule"
            );
        }
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib infer::prompt::tests::every_language_says_the_judgement_is_not_an_artifact`
Expected: FAIL on `En`.

- [x] **Step 3: Add the paragraph to all ten constants**

Insert each paragraph as its own paragraph immediately before the closing sentence of the JUDGE section in that language (the sentence that says to reply with `"artifacts"` alone when there is no JUDGE block).

English, before `With no JUDGE block, reply with "artifacts" alone."#;`:

```text
The judgement is not an artifact. Never write an artifact that describes the
note's intent, restates its dates, or names its relation to a neighbor: those
belong in "moment", "events" and "links" alone. A note of one or two sentences
yields exactly one artifact, in the language the note is written in.

```

German, before `Ohne JUDGE-Block antworte allein mit "artifacts"."#;`:

```text
Das Urteil ist kein Artefakt. Schreibe nie ein Artefakt, das die Absicht der
Notiz beschreibt, ihre Daten wiederholt oder ihre Beziehung zu einem Nachbarn
benennt: das gehört allein in "moment", "events" und "links". Eine Notiz von
ein oder zwei Sätzen ergibt genau ein Artefakt, in der Sprache der Notiz.

```

Spanish, before `Sin bloque JUDGE, responde solo con "artifacts"."#;`:

```text
El juicio no es un artefacto. Nunca escribas un artefacto que describa la
intención de la nota, repita sus fechas o nombre su relación con un vecino:
eso va solo en "moment", "events" y "links". Una nota de una o dos frases
produce exactamente un artefacto, en el idioma de la nota.

```

French, before `Sans bloc JUDGE, réponds avec "artifacts" seul."#;`:

```text
Le jugement n'est pas un artefact. N'écris jamais un artefact qui décrit
l'intention de la note, répète ses dates ou nomme sa relation à un voisin :
cela relève uniquement de "moment", "events" et "links". Une note d'une ou
deux phrases donne exactement un artefact, dans la langue de la note.

```

Italian, before `Senza blocco JUDGE, rispondi con "artifacts" da solo."#;`:

```text
Il giudizio non è un artefatto. Non scrivere mai un artefatto che descriva
l'intento della nota, ne ripeta le date o ne nomini la relazione con un
vicino: questo spetta solo a "moment", "events" e "links". Una nota di una o
due frasi produce esattamente un artefatto, nella lingua della nota.

```

Dutch, before `Zonder JUDGE-blok antwoord je alleen met "artifacts"."#;`:

```text
Het oordeel is geen artefact. Schrijf nooit een artefact dat de bedoeling van
de notitie beschrijft, haar data herhaalt of haar relatie tot een buur noemt:
dat hoort alleen in "moment", "events" en "links". Een notitie van één of
twee zinnen levert precies één artefact op, in de taal van de notitie.

```

Polish, before `Bez bloku JUDGE odpowiadaj samym „artifacts"."#;`:

```text
Ocena nie jest artefaktem. Nigdy nie pisz artefaktu opisującego intencję
notatki, powtarzającego jej daty ani nazywającego jej związek z sąsiadem: to
należy wyłącznie do "moment", "events" i "links". Notatka z jednego lub dwóch
zdań daje dokładnie jeden artefakt, w języku notatki.

```

Portuguese, before `Sem bloco JUDGE, responde apenas com "artifacts"."#;`:

```text
O julgamento não é um artefato. Nunca escreva um artefato que descreva a
intenção da nota, repita as suas datas ou nomeie a sua relação com um vizinho:
isso pertence apenas a "moment", "events" e "links". Uma nota de uma ou duas
frases produz exatamente um artefato, na língua da nota.

```

Russian, before `Без блока JUDGE отвечай одним лишь "artifacts"."#;`:

```text
Суждение — не артефакт. Никогда не пиши артефакт, который описывает намерение
заметки, повторяет её даты или называет её связь с соседом: это место только
для "moment", "events" и "links". Заметка из одного-двух предложений даёт
ровно один артефакт, на языке заметки.

```

Turkish, before `JUDGE bloğu yoksa yalnızca "artifacts" ile yanıtla."#;`:

```text
Yargı bir artefakt değildir. Notun niyetini anlatan, tarihlerini yineleyen ya
da bir komşuyla ilişkisini adlandıran bir artefakt asla yazma: bunlar yalnızca
"moment", "events" ve "links" içindir. Bir ya da iki cümlelik bir not, notun
dilinde tam olarak bir artefakt verir.

```

- [x] **Step 4: Run the prompt tests**

Run: `cargo test --locked --lib infer::prompt`
Expected: PASS, including the existing marker test (`"NEIGHBORS"`, `"JUDGE"` still present in all ten).

- [x] **Step 5: Re-measure the prompt overhead**

`prompt_overhead` in `src/jobs/synthesize.rs` counts the real prompt, so nothing to change. Run `cargo test --locked --lib jobs::synthesize` to confirm budgets still hold.

- [x] **Step 6: Commit**

```bash
git add src/infer/prompt.rs
git commit -m "feat(prompt): the judgement is not an artifact, in all ten languages"
```

---

### Task 3: A named weekday wins over the model's arithmetic

**Files:**
- Modify: `src/jobs/judgement.rs:28-186` (`apply`), add two functions near `parse_local` (`:242`)
- Test: `src/jobs/judgement.rs` tests module

**Interfaces:**
- Produces: `pub(crate) fn weekday_named(text: &str) -> Option<chrono::Weekday>` and `pub(crate) fn onto_named_weekday(at: i64, named: chrono::Weekday, now: i64, tz: chrono_tz::Tz) -> i64`.

- [x] **Step 1: Write the failing tests**

Add to the `tests` module of `src/jobs/judgement.rs`:

```rust
    #[test]
    fn a_weekday_is_read_in_any_of_the_ten_languages_and_only_when_unambiguous() {
        use chrono::Weekday;
        assert_eq!(weekday_named("erinnere mich an den Termin, Freitag 13:45 uhr."), Some(Weekday::Fri));
        assert_eq!(weekday_named("call the dentist on Tuesday"), Some(Weekday::Tue));
        assert_eq!(weekday_named("rappelle-moi jeudi"), Some(Weekday::Thu));
        assert_eq!(weekday_named("cuma günü toplantı"), Some(Weekday::Fri));
        assert_eq!(weekday_named("pazartesi sabah"), Some(Weekday::Mon));
        // Two different days named: no single answer.
        assert_eq!(weekday_named("Montag oder Freitag"), None);
        // No day named.
        assert_eq!(weekday_named("morgen um 9"), None);
        // A weekday inside another word is not a weekday.
        assert_eq!(weekday_named("the monday-ish feeling"), Some(Weekday::Mon));
        assert_eq!(weekday_named("sundays"), None);
    }

    #[test]
    fn a_resolved_date_is_moved_onto_the_named_weekday() {
        use chrono::{TimeZone, Weekday};
        let tz = chrono_tz::Europe::Berlin;
        let now = tz.with_ymd_and_hms(2026, 9, 2, 14, 43, 0).unwrap().timestamp(); // Wednesday
        let sat = tz.with_ymd_and_hms(2026, 9, 5, 13, 45, 0).unwrap().timestamp();
        let fri = tz.with_ymd_and_hms(2026, 9, 4, 13, 45, 0).unwrap().timestamp();
        assert_eq!(onto_named_weekday(sat, Weekday::Fri, now, tz), fri);
        // Already right: untouched.
        assert_eq!(onto_named_weekday(fri, Weekday::Fri, now, tz), fri);
        // Named the day of capture itself: next week, not a past hour today.
        let next_wed = tz.with_ymd_and_hms(2026, 9, 9, 13, 45, 0).unwrap().timestamp();
        assert_eq!(onto_named_weekday(sat, Weekday::Wed, now, tz), next_wed);
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked --lib jobs::judgement::tests::a_weekday_is_read`
Expected: compile error, `weekday_named` not found.

- [x] **Step 3: Implement the two functions**

Add above `fn naive(` in `src/jobs/judgement.rs`:

```rust
/// The weekday a note names, in any of the ten prompt languages.
///
/// `None` when it names none, and `None` when it names two different ones —
/// "Montag oder Freitag" is a question, not a date. Whole words only: the
/// text is split on anything that is not a letter, digit or hyphen, and
/// "sundays" is not "sunday". The hyphen stays a word character for the
/// Portuguese "sexta-feira".
pub(crate) fn weekday_named(text: &str) -> Option<chrono::Weekday> {
    use chrono::Weekday::*;
    const NAMES: &[(&str, chrono::Weekday)] = &[
        ("monday", Mon), ("montag", Mon), ("lunes", Mon), ("lundi", Mon), ("lunedì", Mon),
        ("lunedi", Mon), ("maandag", Mon), ("poniedziałek", Mon), ("segunda-feira", Mon),
        ("segunda", Mon), ("понедельник", Mon), ("pazartesi", Mon),
        ("tuesday", Tue), ("dienstag", Tue), ("martes", Tue), ("mardi", Tue), ("martedì", Tue),
        ("martedi", Tue), ("dinsdag", Tue), ("wtorek", Tue), ("terça-feira", Tue), ("terça", Tue),
        ("вторник", Tue), ("salı", Tue),
        ("wednesday", Wed), ("mittwoch", Wed), ("miércoles", Wed), ("miercoles", Wed),
        ("mercredi", Wed), ("mercoledì", Wed), ("mercoledi", Wed), ("woensdag", Wed), ("środa", Wed),
        ("quarta-feira", Wed), ("quarta", Wed), ("среда", Wed), ("çarşamba", Wed),
        ("thursday", Thu), ("donnerstag", Thu), ("jueves", Thu), ("jeudi", Thu), ("giovedì", Thu),
        ("giovedi", Thu), ("donderdag", Thu), ("czwartek", Thu), ("quinta-feira", Thu),
        ("quinta", Thu), ("четверг", Thu), ("perşembe", Thu),
        ("friday", Fri), ("freitag", Fri), ("viernes", Fri), ("vendredi", Fri), ("venerdì", Fri),
        ("venerdi", Fri), ("vrijdag", Fri), ("piątek", Fri), ("sexta-feira", Fri), ("sexta", Fri),
        ("пятница", Fri), ("cuma", Fri),
        ("saturday", Sat), ("samstag", Sat), ("sonnabend", Sat), ("sábado", Sat), ("sabado", Sat),
        ("samedi", Sat), ("sabato", Sat), ("zaterdag", Sat), ("sobota", Sat), ("суббота", Sat),
        ("cumartesi", Sat),
        ("sunday", Sun), ("sonntag", Sun), ("domingo", Sun), ("dimanche", Sun), ("domenica", Sun),
        ("zondag", Sun), ("niedziela", Sun), ("воскресенье", Sun), ("pazar", Sun),
    ];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    let mut found: Option<chrono::Weekday> = None;
    for (name, day) in NAMES {
        if words.iter().any(|w| w == name) {
            if found.is_some_and(|f| f != *day) {
                return None;
            }
            found = Some(*day);
        }
    }
    found
}

/// The instant the model resolved, moved onto the weekday the note names.
///
/// The model does the calendar arithmetic and gets it wrong by a day often
/// enough — "Freitag" on a Wednesday came back as the Saturday. The note
/// itself is the stronger witness: when it names a weekday and the resolved
/// instant falls on another, the date becomes the first such weekday after
/// the capture, at the time of day the model resolved. A weekday that is the
/// capture's own day means next week. `at` unchanged when the two agree or
/// the zone cannot place either instant.
pub(crate) fn onto_named_weekday(
    at: i64,
    named: chrono::Weekday,
    now: i64,
    tz: chrono_tz::Tz,
) -> i64 {
    use chrono::{Datelike, Duration, TimeZone};
    let Some(local) = tz.timestamp_opt(at, 0).single() else {
        return at;
    };
    if local.weekday() == named {
        return at;
    }
    let Some(today) = tz.timestamp_opt(now, 0).single().map(|d| d.date_naive()) else {
        return at;
    };
    let ahead = (i64::from(named.num_days_from_monday())
        - i64::from(today.weekday().num_days_from_monday()))
    .rem_euclid(7);
    let ahead = if ahead == 0 { 7 } else { ahead };
    let date = today + Duration::days(ahead);
    tz.from_local_datetime(&date.and_time(local.time()))
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(at)
}
```

- [x] **Step 4: Apply it in `apply`**

In `src/jobs/judgement.rs`, inside `apply`, add after `let tz_name = tz.name().to_string();`:

```rust
    // The note's own weekday, read once for the events and the reminder
    // below. See `onto_named_weekday`.
    let named_day = weekday_named(&src.raw_text);
    let reconcile = |at: i64| match named_day {
        Some(d) => {
            let moved = onto_named_weekday(at, d, src.created_at, tz);
            if moved != at {
                tracing::warn!(
                    corpus_id,
                    from = at,
                    to = moved,
                    weekday = ?d,
                    "the model's date fell on another weekday than the note names; moved"
                );
            }
            moved
        }
        None => at,
    };
```

Then change the events loop's first line from

```rust
        let Some(at) = parse_local(e, tz) else {
```

to

```rust
        let Some(at) = parse_local(e, tz).map(reconcile) else {
```

and in the `Some("remind")` arm change

```rust
            let at = j.when.as_deref().and_then(|w| parse_local(w, tz));
```

to

```rust
            let at = j.when.as_deref().and_then(|w| parse_local(w, tz)).map(reconcile);
```

`src` is the corpus row already loaded at the top of `apply`; `src.raw_text` and `src.created_at` exist on it (see `build_judge_ask` in `window.rs` for the same use).

- [x] **Step 5: Run the tests and lint**

Run: `cargo test --locked --lib jobs::judgement && cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS. If clippy flags the closure borrowing `src` across an await, change `let reconcile = |at: i64|` to `let reconcile = move |at: i64|` after copying `let created_at = src.created_at;` above it and using `created_at` inside.

- [x] **Step 6: Commit**

```bash
git add src/jobs/judgement.rs
git commit -m "fix(judgement): a weekday the note names wins over the model's calendar arithmetic"
```

---

### Task 4: Coverage prefers a placed artifact on a tie

**Files:**
- Modify: `src/jobs/promote.rs:102-119` (`covered_by`)
- Test: `src/jobs/promote.rs` tests module, next to `the_majority_rule_is_per_artifact_best_overlap_ties_to_the_lowest_ordinal`

**Interfaces:**
- Consumes: `CorpusSpan::places_the_artifact(&self) -> bool`, `CorpusSpan::claimed(a, b)`, `CorpusSpan::unplaced(a, b)` (all exist in `src/store/artifacts.rs`).

- [x] **Step 1: Write the failing test**

```rust
    #[test]
    fn on_equal_overlap_a_placed_artifact_beats_an_unplaced_one_whatever_its_ordinal() {
        let passages = vec![("p".to_string(), sp(1, 1))];
        // Ordinal 1 is unplaced — the fallback span that covers the whole
        // window and locates nothing. Ordinal 2 is a claim about line 1.
        let arts = vec![
            ("u".to_string(), 1, CorpusSpan::unplaced(1, 1)),
            ("c".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "c")]);
        // Two placed: the lowest ordinal still wins.
        let arts = vec![
            ("y".to_string(), 3, CorpusSpan::claimed(1, 1)),
            ("x".to_string(), 2, CorpusSpan::claimed(1, 1)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "x")]);
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib jobs::promote::tests::on_equal_overlap_a_placed`
Expected: FAIL, got `("p", "u")`.

- [x] **Step 3: Change the sort key**

Replace the body of the `for (pid, ps) in passages` loop in `covered_by`:

```rust
    for (pid, ps) in passages {
        let len = ps.end_line - ps.start_line + 1;
        // Best overlap; among equals a placed span before an unplaced one,
        // because an unplaced span is the whole-window fallback and says
        // nothing about *which* passage — and `supersede_covered` will not
        // read the vector for it. Then the lowest ordinal.
        let best = artifacts
            .iter()
            .map(|(aid, ord, asp)| (overlap(ps, asp), asp.places_the_artifact(), *ord, aid.as_str()))
            .filter(|(ov, _, _, _)| 2 * ov > len)
            .max_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)).then(y.2.cmp(&x.2)));
        if let Some((_, _, _, aid)) = best {
            out.push((pid.as_str(), aid));
        }
    }
```

Update the doc comment above `covered_by` with one sentence: "Ties on overlap go to a placed span, then to the lowest ordinal."

- [x] **Step 4: Run the tests and lint**

Run: `cargo test --locked --lib jobs::promote && cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS, including the existing tie test (its spans are all `located`, so placed-ness is equal and the ordinal rule still decides).

- [x] **Step 5: Commit**

```bash
git add src/jobs/promote.rs
git commit -m "fix(promote): a placed artifact covers a passage before an unplaced one does"
```

---

### Task 5: The note's language, read from the note

> **Dropped on the operator's instruction, 2026-09-02.** The capture language
> stays what the Settings choice or the browser's `Accept-Language` says. A
> detector reading the text would be a third, arbitrary source of the answer and
> one more thing for a reader to hold in their head. Spec finding 4 stands
> unaddressed by decision, and finding 2's cross-language half with it.


**Files:**
- Modify: `src/infer/lang.rs` (add `detect`), `src/core/ingest.rs:190-201` (`Capture::with_lang`), `src/web/templates/settings.html:28` (copy)
- Test: `src/infer/lang.rs` tests module; `src/core/ingest.rs` tests module

**Interfaces:**
- Produces: `pub fn detect(text: &str) -> Option<Lang>` in `src/infer/lang.rs`.

- [ ] **Step 1: Write the failing tests**

In `src/infer/lang.rs` tests:

```rust
    #[test]
    fn a_note_is_read_in_its_own_language_when_it_has_enough_words_to_tell() {
        assert_eq!(detect("erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr."), Some(Lang::De));
        assert_eq!(detect("remind me about the dentist on Friday at two"), Some(Lang::En));
        assert_eq!(detect("rappelle-moi de payer la facture pour le garage"), Some(Lang::Fr));
        assert_eq!(detect("напомни мне в пятницу о встрече с врачом"), Some(Lang::Ru));
        // Too short to tell: no opinion.
        assert_eq!(detect("ok"), None);
        assert_eq!(detect("Gastroenterologe 13:45"), None);
        // A bare command line has no function words in any language.
        assert_eq!(detect("sudo systemctl restart engram.service"), None);
    }
```

In `src/core/ingest.rs` tests (find the existing `mod tests` at the end of the file):

```rust
    #[test]
    fn the_language_stamp_follows_the_text_and_falls_back_to_the_door() {
        use crate::infer::lang::Lang;
        let c = Capture::new("erinnere mich an den Termin am Freitag um 13:45", "web")
            .with_lang(Lang::En);
        assert_eq!(c.metadata["lang"], "de");
        let c = Capture::new("ok", "web").with_lang(Lang::En);
        assert_eq!(c.metadata["lang"], "en");
    }
```

(`Capture::new(text, origin)` is at `src/core/ingest.rs:149`.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked --lib infer::lang::tests::a_note_is_read`
Expected: compile error, `detect` not found.

- [ ] **Step 3: Implement `detect`**

Add to `src/infer/lang.rs` after `of_corpus`:

```rust
/// The language a text is written in, read from its function words.
///
/// Ten short lists of the words that carry no meaning and every sentence
/// needs — articles, pronouns, the commonest prepositions. The list with the
/// most whole-word hits wins; fewer than two hits, or fewer than three words
/// in the text, is no opinion, because one "a" or one "die" is a coincidence
/// in any language. Ties go to the earlier list, which puts English first
/// and Spanish before Portuguese — a choice, and a documented one.
///
/// Not a general language identifier and not meant as one: it exists so that
/// a German note captured from an English browser is read in German. Where
/// it has no opinion the door's language stands, which is what `with_lang`
/// does with the result.
pub fn detect(text: &str) -> Option<Lang> {
    const MARKERS: &[(Lang, &[&str])] = &[
        (Lang::En, &["the", "and", "is", "to", "of", "with", "that", "for", "this", "at", "me", "about", "on"]),
        (Lang::De, &["der", "die", "das", "und", "ist", "nicht", "mit", "ich", "den", "dem", "ein", "eine", "auf", "für", "um", "an", "zu", "mich", "am", "im"]),
        (Lang::Es, &["el", "la", "los", "las", "y", "es", "con", "que", "para", "por", "una", "del", "al", "me"]),
        (Lang::Fr, &["le", "la", "les", "et", "est", "des", "une", "pour", "que", "dans", "pas", "avec", "au", "du", "moi", "de"]),
        (Lang::It, &["il", "la", "gli", "le", "e", "è", "con", "che", "per", "una", "del", "della", "non", "di", "mi"]),
        (Lang::Nl, &["de", "het", "een", "en", "is", "niet", "met", "van", "voor", "dat", "ik", "op", "mij"]),
        (Lang::Pl, &["i", "jest", "nie", "się", "na", "do", "z", "że", "to", "w", "o", "mi", "przypomnij"]),
        (Lang::Pt, &["o", "a", "os", "as", "e", "é", "com", "que", "para", "uma", "não", "do", "da", "em", "me"]),
        (Lang::Ru, &["и", "в", "не", "на", "что", "это", "с", "я", "как", "по", "к", "мне", "о"]),
        (Lang::Tr, &["ve", "bir", "bu", "için", "ile", "de", "da", "mi", "ne", "çok", "bana", "hatırlat"]),
    ];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .collect();
    if words.len() < 3 {
        return None;
    }
    let mut best: Option<(usize, Lang)> = None;
    for (lang, markers) in MARKERS {
        let hits = words.iter().filter(|w| markers.contains(w)).count();
        if hits >= 2 && best.is_none_or(|(h, _)| hits > h) {
            best = Some((hits, *lang));
        }
    }
    best.map(|(_, l)| l)
}
```

(`Option::is_none_or` is stable since 1.82; the crate's `rust-version` is 1.94.)

- [ ] **Step 4: Use it at the door**

In `src/core/ingest.rs`, change `with_lang`:

```rust
    pub fn with_lang(mut self, lang: crate::infer::lang::Lang) -> Self {
        // The text outranks the door. A German note pasted from an English
        // browser was read — and rewritten — in English, and the operator
        // filed a bug report saying so. Where the text is too short to tell,
        // the door's language stands.
        let lang = crate::infer::lang::detect(&self.text).unwrap_or(lang);
        self.metadata["lang"] = serde_json::Value::String(lang.tag().to_string());
        self
    }
```

Update the doc comment above it: replace the sentence "On the corpus it is also the more truthful place — this is the language the capture was made in, which is what the reading should follow." with "The text itself is asked first (`lang::detect`); the door's language is the fallback for text too short to tell."

- [ ] **Step 5: Adjust the Settings copy**

In `src/web/templates/settings.html` line 28, change the paragraph beginning `Which language your captures are read and written in` to:

```html
<p class="muted">Which language your captures are read and written in when the
  text itself does not say — a note written in German is read in German
  whatever is chosen here. The prompt, the artifacts and their titles follow;
  the interface stays English.</p>
```

(Keep whatever the rest of that paragraph currently says about the interface if it differs; only the first sentence changes.)

- [ ] **Step 6: Run tests and lint**

Run: `cargo test --locked --lib infer::lang && cargo test --locked --lib core::ingest && cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/infer/lang.rs src/core/ingest.rs src/web/templates/settings.html
git commit -m "feat(lang): a capture is read in the language it is written in, the door's language as fallback"
```

---

## Part B — the workspace

Verification for template and CSS tasks: `cargo test --locked` (askama compiles templates at build time, and `src/web/ui.rs` has render tests), then run the app and look. To run locally: `cargo run --release -- --config config.toml` with a `config.toml` copied from `config.example.toml`; open `http://127.0.0.1:8080/ui`. A phone check is Chrome devtools at 390 px width.

### Task 6: One verb carries the accent, and a disabled verb never looks lit

**Files:**
- Modify: `assets/css/30-components.css:13-15`, `src/web/templates/_ask_verb.html` (the `<button ... data-verb="ask"` line), `assets/app.js:1096-1110` (`sync()` in the verb-arming function), `src/web/templates/_keyhint.html` (the `keep` label)

- [x] **Step 1: CSS — disabled accent reads disabled**

In `assets/css/30-components.css`, after `.btn:disabled { opacity: 0.4; pointer-events: none; }` add:

```css
/* A disabled verb is grey, whatever it would be when armed. The accent at
   0.4 opacity still read as the lit primary on an empty box — the one state
   in which nothing on the page should say "press me". */
.btn-accent:disabled {
  background: var(--color-bg-active); color: var(--color-fg-muted);
  border-color: var(--color-border-strong); font-weight: 500;
}
```

- [x] **Step 2: Template — Ask starts plain**

In `src/web/templates/_ask_verb.html` change `class="btn btn-accent"` on the Ask button to `class="btn"`. Add a comment line above it inside the existing comment block: `app.js lends the accent to whichever verb the box's content shape suggests; see the verb-arming sync().`

- [x] **Step 3: JS — lend the accent by content shape**

In `assets/app.js`, inside `function sync()` (the one that reads `hasText` and `hasFile`), replace the `for` loop with:

```js
      // Which verb the accent goes to. Not which verb runs — that stays a
      // press — only which one is lit. A trailing question mark or a leading
      // question word says Ask; a paste (long, multi-line, or a file) says
      // Capture; a short plain sentence lights neither, because typing
      // already searches and there is nothing to press for.
      var text = box.value.trim();
      var asksLike = /\?\s*$/.test(text) ||
        /^(who|what|when|where|why|how|which|is|are|do|does|did|can|could|should|wer|was|wann|wo|warum|wie|welche|ist|sind|kann|hat|habe)\b/i.test(text);
      var keepsLike = hasFile || text.length > 200 || text.indexOf('\n') !== -1;
      for (var i = 0; i < buttons.length; i++) {
        var verb = buttons[i].getAttribute('data-verb');
        buttons[i].disabled = verb === 'capture'
          ? !(hasText || hasFile)
          // A staged file has made the box that file's note. Asking a note is
          // not a thing to do, and the answer would land beside a file the
          // question was never about.
          : (!hasText || hasFile);
        var lead = verb === 'ask' ? (asksLike && !keepsLike) : keepsLike;
        buttons[i].classList.toggle('btn-accent', lead && !buttons[i].disabled);
      }
```

- [x] **Step 4: One vocabulary**

In `src/web/templates/_keyhint.html` change `<kbd>Ctrl</kbd><kbd>⇧</kbd><kbd>↵</kbd> keep` to `<kbd>Ctrl</kbd><kbd>⇧</kbd><kbd>↵</kbd> capture`.

- [x] **Step 5: Verify**

Run: `cargo test --locked`. Then in the browser: empty box → both verbs grey, neither accented. Type `wie habe ich backups eingerichtet?` → Ask accented. Paste three lines → Capture accented. Attach a file → Capture accented, Ask grey.

- [x] **Step 6: Commit**

```bash
git add assets/css/30-components.css assets/app.js src/web/templates/_ask_verb.html src/web/templates/_keyhint.html
git commit -m "feat(verbs): the accent follows the shape of what is typed, and a disabled verb is grey"
```

---

### Task 7: The cloud steps back when typing starts, and stays off the topbar

**Files:**
- Modify: `assets/css/05-background.css:7-17`, `assets/css/20-layout.css:179`, `assets/app.js:919-938` (`hideIdle`, `showIdle`)

- [x] **Step 1: CSS**

In `assets/css/05-background.css` replace the `opacity: 0.72;` line and its comment with:

```css
  /* Loud enough to be the empty page's picture, and it steps back the moment
     there is text to read: app.js puts `typing` on <html> with the first
     keystroke and takes it off when the box empties. */
  opacity: 0.72;
  transition: opacity 400ms ease;
}
html.typing #vec-bg { opacity: 0.18;
```

(The closing brace of the `#vec-bg` rule follows as before; the result must be two rules: `#vec-bg { … opacity: 0.72; transition: …; }` and `html.typing #vec-bg { opacity: 0.18; }`.)

In `assets/css/20-layout.css` change line 179 to:

```css
/* Painted, not transparent: the projection's axes ran straight through the
   nav, which made the chrome look like part of the picture. */
.topbar { border-bottom: 1px solid var(--color-border); margin-bottom: 1.5rem; background: var(--color-bg-surface); }
```

- [x] **Step 2: JS**

In `assets/app.js`, in `function hideIdle()` add as the first line `document.documentElement.classList.add('typing');` and in `function showIdle()` add as the first line `document.documentElement.classList.remove('typing');`.

- [x] **Step 3: Verify**

Browser: load `/ui` — cloud at full strength, no axis lines through the nav. Type one character — cloud fades. Clear the box — it comes back.

- [x] **Step 4: Commit**

```bash
git add assets/css/05-background.css assets/css/20-layout.css assets/app.js
git commit -m "feat(bg): the cloud fades on the first keystroke and stops behind the topbar"
```

---

### Task 8: Phone first run — the chips stay, the bar sits still while idle

**Files:**
- Modify: `assets/css/50-phone.css:146-150` and the `.regions-rail-focus-source .region-bar` rule directly above it (`:~120-130`)

- [x] **Step 1: The hint hides only while typing**

Replace

```css
  .regions-rail-focus-source .region-bar .hint,
  .regions-rail-focus-source .region-bar .chips,
  .regions-rail-focus-source .region-bar .facet-label,
  .regions-rail-focus-source .region-bar .keyhint,
  .regions-rail-focus-source .region-bar .row { display: none; }
```

with

```css
  .regions-rail-focus-source .region-bar .chips,
  .regions-rail-focus-source .region-bar .facet-label,
  .regions-rail-focus-source .region-bar .keyhint,
  .regions-rail-focus-source .region-bar .row { display: none; }
  /* The hint carries the two example chips, and on a phone they are the
     only thing that says what the box is for — the narrow placeholder is
     clipped around its thirtieth character. Shown while the box is empty,
     gone with the first keystroke like the rest of the idle column. */
  html.typing .regions-rail-focus-source .region-bar .hint { display: none; }
```

- [x] **Step 2: The bar is in the flow while idle**

Directly after the `.regions-rail-focus-source .region-bar { position: fixed; … }` rule add:

```css
  /* Idle, the bar is the page: the idle column lives inside it, and fixed
     to the bottom it left the top two-fifths of the screen empty over a
     box nobody had typed in. It drops to the thumb with the first
     keystroke, which is when a thumb needs it. */
  html:not(.typing) .regions-rail-focus-source .region-bar {
    position: static; border-top: 0; padding: 0; background: transparent;
  }
```

- [x] **Step 3: Verify**

Chrome devtools, 390 px, `/ui` on a held base: box at the top under the nav, "Try … or …" chips visible, no empty band. Type: bar moves to the bottom, chips gone. Clear: back.

- [x] **Step 4: Commit**

```bash
git add assets/css/50-phone.css
git commit -m "feat(phone): the example chips survive to the phone, and the idle bar sits in the flow"
```

---

### Task 9: The rail head counts the loose matches

**Files:**
- Modify: `src/web/ui.rs:565-580` (`ResultsTemplate`), `:1428-1440` (its construction), `:~6924` and `:~7032` (test constructions), `src/web/templates/_results.html:26`

- [x] **Step 1: Write the failing test**

Next to the existing `ResultsTemplate` test at `src/web/ui.rs:~6900`, add a test that builds the template the same way that test does, with three results of which the last has `weak: true`, `all_weak: false`, `loose: 1`, renders it and asserts:

```rust
        let html = t.render().unwrap();
        assert!(html.contains("3 results · 1 loose"), "{html}");
```

Copy the `RenderedResult` literal from that neighbouring test verbatim for the three rows; only `weak` differs on the third.

- [x] **Step 2: Run to verify it fails**

Run: `cargo test --locked --lib web::ui::tests -- loose`
Expected: compile error, no field `loose`.

- [x] **Step 3: Add the field**

In `ResultsTemplate` after `all_weak: bool,` add:

```rust
    /// How many of the ranked results are loose. Said in the heading when
    /// the list is mixed; when every one is loose the flag above the list
    /// already says so and this stays out of the heading.
    loose: usize,
```

In the construction at `:~1435` add `loose: results.iter().filter(|r| r.weak).count(),` on the line before `all_weak:` (both read `results` before it moves). In the two test constructions add `loose: 0,` (the `all_weak: true` one) and `loose: 0,` (the `all_weak: false` one), and `loose: 1,` in the new test.

- [x] **Step 4: Template**

In `src/web/templates/_results.html` line 26, after `{{ results.len() }} result{% if results.len() != 1 %}s{% endif %}` insert `{% if loose > 0 && !all_weak %} · {{ loose }} loose{% endif %}`.

- [x] **Step 5: Run and commit**

Run: `cargo test --locked --lib web::ui && cargo clippy --all-targets --locked -- -D warnings`

```bash
git add src/web/ui.rs src/web/templates/_results.html
git commit -m "feat(rail): the heading counts the loose matches in a mixed list"
```

---

### Task 10: The due badge says its sentence on the row

**Files:**
- Modify: `src/web/templates/_results.html:128-146` (the `rail-why` block)

- [x] **Step 1: Template**

Change the guard `{% if r.primed || r.weak || r.model_written || r.why_ranked.is_some() %}` to `{% if r.primed || r.weak || r.model_written || r.due_in.is_some() || r.why_ranked.is_some() %}`.

After the `model_written` line and before the `why_ranked` line insert:

```
        {%- if let Some(d) = r.due_in %}{% if r.primed || r.weak || r.model_written %} · {% endif %}a reminder on this is due {{ d }}{% endif -%}
```

Change the `why_ranked` separator condition from `{% if r.primed || r.weak || r.model_written %}` to `{% if r.primed || r.weak || r.model_written || r.due_in.is_some() %}`.

- [x] **Step 2: Verify and commit**

Run: `cargo test --locked --lib web::ui`. Browser: search for the Gastro note; its row reads "a reminder on this is due …" under the snippet.

```bash
git add src/web/templates/_results.html
git commit -m "feat(rail): a due reminder is a sentence on the row, not a tooltip"
```

---

### Task 11: The empty base says one thing once

**Files:**
- Modify: `src/web/templates/_box_hint.html` (the `{%- else -%}` branch), `src/web/templates/_idle_foot.html` (the `{%- if !held %}` branch), `src/web/templates/workspace.html` (the two `placeholder` attributes' `{% else %}` halves), `assets/css/40-workspace.css` (one rule)

- [x] **Step 1: The hint carries the sentence**

In `_box_hint.html` replace the not-held branch text with:

```
  Paste anything worth keeping &mdash; a note, an article, a chunk of a chat.
  engram finds it again by meaning, and nobody else can search this base.
```

- [x] **Step 2: The foot says nothing on an empty base**

In `_idle_foot.html` replace the two-line "Nothing here yet. Paste anything worth keeping …" text inside `{%- if !held %}` with nothing (keep the branch and its comment; the paragraph renders empty). Add to `assets/css/40-workspace.css` next to the `.idle-foot` rule (grep `idle-foot`):

```css
.idle-foot:empty { display: none; }
```

- [x] **Step 3: The placeholder stops repeating the hint**

In `workspace.html` change the not-held placeholder from `Paste anything worth keeping — a note, an article, a chunk of a chat.` to `Paste something to keep…` and the not-held narrow one from `Paste anything worth keeping…` to `Paste something to keep…`.

- [x] **Step 4: Verify and commit**

Run `cargo test --locked --lib web::`. Browser with an empty base (a fresh tenant, or `open_registration` sign-in with a new account): one placeholder, one sentence under the box, nothing else.

```bash
git add src/web/templates/_box_hint.html src/web/templates/_idle_foot.html src/web/templates/workspace.html assets/css/40-workspace.css
git commit -m "fix(idle): an empty base introduces itself once"
```

---

### Task 12: The corpus page stops measuring the passage against itself

**Files:**
- Modify: `src/web/ui.rs` — the corpus page handler that builds `CorpusTemplate` (construction at `:~2061`), `fn artifact_view` at `:464`, `src/web/templates/_artifact.html:1-5`

- [x] **Step 1: Coverage is `None` for a verbatim-only corpus**

In the handler, find the `let coverage` binding (run `grep -n "coverage" src/web/ui.rs` and take the one inside the function that ends with `Ok(HtmlTemplate(CorpusTemplate {`). The handler already holds the corpus's artifacts as a `Vec<Chunk>` to build `bands`; call that vector by its name in the code below. Wrap the binding:

```rust
    // A verbatim capture's passages *are* the wording, so the measure is
    // 100% by construction and says nothing. Stated only once something was
    // written from the source.
    let coverage = if chunks
        .iter()
        .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
    {
        None
    } else {
        coverage
    };
```

- [x] **Step 2: A passage card has no title**

In `fn artifact_view`, change `title: artifact_title(c),` to:

```rust
        // A passage has no title by design and its first line is its body:
        // shown as both, the card said everything twice.
        title: if c.provenance == crate::store::artifacts::Provenance::Passage && c.title.is_none() {
            String::new()
        } else {
            artifact_title(c)
        },
```

In `_artifact.html` wrap the title span: `{% if !c.title.is_empty() %}<span class="card-title">{{ c.title }}</span>{% endif %}`.

- [x] **Step 3: Verify and commit**

Run `cargo test --locked --lib web::ui`. Browser: the Gastro corpus page shows no coverage line once its rewrites are gone, and the passage card shows the sentence once.

```bash
git add src/web/ui.rs src/web/templates/_artifact.html
git commit -m "fix(corpus): no coverage line for a verbatim-only capture, no title on a passage card"
```

---

### Task 13: The offer card explains itself in a sentence

**Files:**
- Modify: `src/web/ui.rs:1149-1170` (`offer_view`), `src/web/templates/_context.html` (the `offer-why` div and the `offer-detail` details)

- [x] **Step 1: Sentences instead of rung names**

In `offer_view` replace the `rung:` match arms:

```rust
        rung: match o.rung {
            Rung::Pattern => "Offered because you tend to open things like this around now — like".to_string(),
            Rung::Similar => "Offered because it is like what you opened".to_string(),
            Rung::Tentative => match o.events {
                0 | 1 => "Offered on one earlier occasion like this —".to_string(),
                2 => "Offered on two earlier occasions like this —".to_string(),
                n => format!("Offered on {n} earlier occasions like this —"),
            },
            // Nothing about the situation produced it, so nothing is claimed.
            Rung::Random => String::new(),
        },
```

- [x] **Step 2: The blocks move into Details**

In `_context.html` replace the `offer-why` div's content with:

```html
  <div class="muted offer-why">{{ o.rung }}{% if !o.when.is_empty() %} {{ o.when }}{% endif %}</div>
```

and change the `<pre class="mono">{{ o.detail }}</pre>` inside `offer-detail` to:

```html
      <pre class="mono">{% if !o.blocks.is_empty() %}signals: {{ o.blocks }}
{% endif %}{{ o.detail }}</pre>
```

- [x] **Step 3: Verify and commit**

The test at `src/web/ui.rs:5911` asserts `body.contains("Pattern")`; change it to `body.contains("Offered because")`. Then run `cargo test --locked --lib web::`. Browser: the card reads "Offered because you tend to open things like this around now — like 26.08., 20:36".

```bash
git add src/web/ui.rs src/web/templates/_context.html
git commit -m "feat(offer): the card says why in a sentence; the signals move into details"
```

---

### Task 14: The idle foot's link is one line

**Files:**
- Modify: `assets/css/40-workspace.css` (next to the `.idle-foot` rule)

- [x] **Step 1: CSS**

Replace the existing `.idle-foot a { color: inherit; }` at `assets/css/40-workspace.css:1021` with:

```css
/* The last kept note is a link, and a note's first line can be a paragraph.
   One line, cut with an ellipsis; the title on the far side is the same
   text in full. */
.idle-foot a {
  color: inherit; display: inline-block; max-width: 20rem; vertical-align: bottom;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
```

- [x] **Step 2: Verify and commit**

Browser at 390 px: the foot line is two lines at most, the link ends in "…".

```bash
git add assets/css/40-workspace.css
git commit -m "fix(idle): the last-kept link is one line"
```

---

### Task 15: Full verification and the prod re-run

- [x] **Step 1: Whole suite**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: clean.

Ran 2026-09-02, all three clean: `cargo fmt --all -- --check`,
`cargo clippy --all-targets --locked -- -D warnings` (exit 0, no warnings), and
`cargo test --locked` (2360 passed, 0 failed).

- [ ] **Step 2: Reproduce the original capture locally**

With the app running against a fake or real synthesizer, capture `erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr.` on a Wednesday (or set the machine clock) and confirm in the journal:

```
more artifacts than the window can carry   (only if the model still over-delivers)
promotion superseded its covered passages ... superseded=1
```

and on the corpus page: one artifact, in German, the passage hidden, the due band naming a Friday.

- [ ] **Step 3: Deploy**

Push the branch; on prod `git pull && ./build-lowmem.sh && systemctl restart engram` as the existing deploy reflog shows. Re-capture the same sentence and read the same two journal lines.

---

## Self-review

**Spec coverage.** Bugs 1 and 5 → Tasks 1, 2. Bug 2 → Task 4 (tie rule) and Task 5 (same-language rewrites make the vector rule reachable). Bug 3 → Task 3. Bug 4 → Task 5. UI: primary/disabled → Task 6; canvas and topbar → Task 7; phone chips and empty band → Task 8; rail counts → Task 9; due tooltip → Task 10; empty-base copy → Task 11; corpus coverage and passage title → Task 12; offer meta line → Task 13; idle foot link → Task 14; keep/capture vocabulary → Task 6 step 4. Deferred items are listed in the spec with reasons.

**Placeholders.** Task 12 step 1 names the vector by role rather than by identifier because the handler's local name was not read; the grep and the construction site line are given. Task 5's `Capture::new(text, origin)` matches `src/core/ingest.rs:149`.

**Type consistency.** `artifact_allowance`/`within_allowance` (Task 1) take `usize` and `Vec<ProposedArtifact>`; `weekday_named`/`onto_named_weekday` (Task 3) return `Option<Weekday>`/`i64`; `detect` (Task 5) returns `Option<Lang>`; `loose: usize` (Task 9) is the same name in struct, construction, tests and template.
