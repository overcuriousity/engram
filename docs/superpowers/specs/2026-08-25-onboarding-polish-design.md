# Onboarding and wording polish

**Status:** design, awaiting review
**Date:** 2026-08-25

## The situation

After #52 every user gets their own database and their own collection. The
application is about to be opened to more than one person for the first time,
and there is no first-run experience of any kind in the tree — no `welcome`, no
`onboard`, no seen-flag, nothing. A new user arrives through their identity
provider, presses Continue, and lands on `/ui` with an empty base, a box
offering three verbs, and one sentence in the rail.

Two of those three verbs cannot work. Searching an empty base returns nothing;
asking an empty base returns an abstention. The one verb that can work —
Capture — is the one the placeholder mentions last.

Nothing on that screen says what engram is. The tagline lives on `login.html`,
which an OIDC user never sees. Nothing says the base is theirs alone, which is
the exact hesitation a person has before pasting their own notes into a server
somebody else runs.

And once they do paste something, the receipt is a dead one-liner — "Captured —
view source" — while embedding runs as a background job. A user who immediately
searches for what they just pasted and gets nothing back concludes the thing is
broken. That is the most likely abandonment moment in the application, and the
fragment that would answer it already exists.

## The principle

Onboarding is a property of an empty base, not of a new user.

No welcome page, no scripted sequence, no per-user "has seen it" flag. Each
surface asks the state it can already see and renders accordingly. This is what
`_rail_idle.html` already does, generalised to the rest of the app.

The consequence worth having: the instruction cannot drift out of sync with
reality, it serves someone who deleted everything just as well as someone new,
and there is no stored flag to migrate, expire, or get wrong.

## 1. First run, as rendered state

### The workspace with nothing held (`corpora == 0`)

**Ask is not rendered.** Not disabled — absent. A disabled button is a promise
the page cannot keep, and Ask on an empty base can only abstain. It appears when
there is something to ask about.

**The placeholder names the one verb that works.** Today it names all three:
"Describe the situation, ask a question, or paste anything worth keeping…". On
an empty base it becomes "Paste anything worth keeping — a note, an article, a
chunk of a chat.", with a narrow variant for the phone as the box already has.

**The hint under the box carries what the login card says and an OIDC user never
sees:** what engram does, and that this base is theirs alone. The privacy
statement goes here rather than on Settings, because here is where the eye
already is and there is where nobody looks before pasting.

**The keyboard hint bar does not render.** Seven shortcuts for moving through a
list that has nothing in it. It returns with the first source.

**The pane stops giving an instruction that cannot be followed.** Today it says
"Search to see an artifact here, beside the lines it came from" — to a person
with nothing to search. It says instead what will happen to the first thing
pasted: split into passages, embedded, the original kept and served back
untouched.

**The rail keeps its current sentence.** It is good copy; it simply stops being
the only thing doing the job.

### While a capture is being read

`_captured.html` includes `_queue.html`, unchanged.

The queue fragment already reports per-source progress — *segmenting 3/7*, the
in-flight dot, *parked*, *failed* — already polls itself every three seconds,
and already stops polling when nothing is moving, so an idle page in a
background tab makes no requests. It listens for the `captured` event on `body`
that the box already fires. It is presently rendered only on Insights, and
`_captured.html`'s own comment records the cost of that: "nothing else on the
workspace says the paste landed."

The gap between *captured* and *searchable* stops being invisible, and it costs
one include.

### Insights with nothing held

The page collapses to one honest line and a way back to the box. Today a new
user is shown Held 0, an empty Reach, a Retrieval panel explaining there is
nothing to measure, and "0 artifacts, 0 embedded. No jobs queued." — a page of
zeros that reads as a system with something wrong with it.

### Judge with an empty queue

Keeps "Nothing to judge", and gains one sentence on what the page is for — these
are your own searches, coming back unlabelled — so an empty visit still teaches
rather than reading as a dead end.

## 2. Naming and explanation

**"Artifact" stays.** It is precise, deliberate, and defended in the comments.
The problem it causes is not the word but the absence of a gloss, and a gloss is
cheaper than a rename.

**`corpus`/`corpora` becomes "source" in user-visible text.** This is not
introducing a third word — it is picking one of two the UI already uses
interchangeably for the same thing. Insights says "from N sources", the idle
rail says "sources", `corpus.html` says "this capture's wording", and the URL
says `corpora`. Roughly forty-five occurrences across the templates. URLs, API
field names, MCP schemas and Rust internals are untouched.

**Judge becomes "Review searches"** in the top row and "Review" on the tabbar.
The present label names a role, not a task, and the badge invites a click into a
page whose purpose is never stated. The duplicate-pair queue takes "decisions",
so the two stop competing for the same word.

**Mint becomes Create.**

**`extension.html` and `pair.html` both say tokens appear "under Housekeeping →
API tokens".** Housekeeping is a heading on Insights; tokens are on Settings.
This is a stale cross-reference, not a matter of taste.

**Every badge gets a visible plain-language line. Never a `title=`.** A tooltip
is invisible on touch, and the terms that most need explaining are the ones a
phone user meets first. `_results.html` already has the correct pattern in
`rail-why`: the badge stays as the scannable form, and one quiet sentence under
it says the thing in words. That pattern extends to `_artifact_detail.html`'s
icon-only buttons, `corpus.html`, and the judge card.

Terms in scope: loose, primed, model-written, "Relevance falls off here",
superseded, deprecated, parked, sweep, rung, pursuit, gap, recall@10, MRR.

## 3. Insights, halved in place

Insights answers two different questions with one page: *what is in my memory and
what needs me*, and *what is the machine doing*. The second is operator-grade —
sweep history with stage identifiers, retrying jobs with target identifiers and
raw error strings, offer rates by rung — and every user now sees it.

The second half moves into a closed `<details>` at the foot of the page. Not a
new route: a new page would need a handler, a template, a link, and its own
empty state, to achieve a separation a disclosure achieves outright.

Above the fold: Held, Reach, Retrieval, Needs you, Knowledge gaps, Merged,
Generated, Hidden as stale, Hidden as near-identical, Worth a second look,
Captures waiting on a decision.

Inside the disclosure: Housekeeping counts, The last day and its sweep history,
Retrying, What was offered.

## 4. Interaction

**Who you are appears in the top bar.** Sign out sits under it rather than at
equal weight beside a theme toggle. The application has never needed this
before; it does now.

**Settings becomes reachable on the phone.** The tabbar has three entries and
Settings is a quiet link at the foot of Insights, which is to say unreachable
for anyone who does not already know where it is.

**Capture and Ask say why they are disabled.** A permanently dead button with no
stated reason is indistinguishable from a broken one.

**The box says that typing searches.** The present hint discusses phrasing — "A
sentence or a whole paragraph finds more than keywords do" — which is true and
is not the thing a first-time user does not know. What they do not know is that
the results appearing beside them are a search they did not press anything to
run.

**The judge card gets one line of motivation.** It asks a real cognitive task —
twenty unordered candidates, "which of these was the one you needed?" — with no
statement of why or what it is worth. These are the user's own searches, coming
back unlabelled, and judging them is what makes the number on Insights mean
anything at all.

**The feedback-purge confirmation becomes two short sentences.** It is presently
one forty-word question, which is a sentence nobody finishes reading before
pressing a button.

**Attach states its accepted types visibly**, rather than in a `title` nobody on
a touch screen can reach.

## Testing

`src/web/ui.rs`, `src/web/workspace.rs` and `src/web/insights.rs` each carry an
inline `#[cfg(test)] mod tests` that renders real structs through
`askama::Template::render` and asserts over the HTML, with helpers in
`test_support.rs`. That is the vehicle; no new test file.

- An empty-base workspace renders no Ask button and no keyhint bar.
- A workspace with sources held renders both.
- The capture receipt renders the queue fragment.
- Empty-base Insights renders the one line and none of the zero tables.
- The operator tables render inside the disclosure and not above it.
- No user-visible template renders "corpus", "corpora", "Mint", or
  "Housekeeping →".

## Out of scope

Renaming "artifact". Any URL, API field, or MCP schema change. Replacing
`window.confirm`. A welcome page, a tour, a seen-flag, or any per-user
onboarding state.
