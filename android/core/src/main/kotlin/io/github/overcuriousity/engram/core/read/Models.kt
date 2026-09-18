package io.github.overcuriousity.engram.core.read

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject

/**
 * The API's shapes, as `docs/api.md` states them and as
 * `src/web/android_fixtures.rs` proves them: the fixtures these are tested
 * against are this server's own answers.
 *
 * Every field a newer server might add is ignored and every field an older one
 * might omit has a default, so neither end has to be updated first.
 */
val ApiJson = Json { ignoreUnknownKeys = true; explicitNulls = false; coerceInputValues = true }

/** Every list the API answers. `next` goes back as `after`; null means there is no more. */
@Serializable
data class Page<T>(val items: List<T> = emptyList(), val next: String? = null)

/** One search result, or one row of "worth seeing again", or one citation. */
@Serializable
data class Hit(
    @SerialName("artifact_id") val artifactId: String,
    @SerialName("corpus_id") val corpusId: String = "",
    val title: String? = null,
    val text: String = "",
    val category: String? = null,
    val tags: List<String> = emptyList(),
    /** Only loosely related to the query. Drawn as a badge in place of a rank. */
    val weak: Boolean = false,
    /** Below the point where this list's relevance falls off. What the divider is drawn from. */
    @SerialName("past_cliff") val pastCliff: Boolean = false,
    val retired: Boolean = false,
    val primed: Boolean = false,
    @SerialName("in_sitting") val inSitting: Boolean = false,
    @SerialName("model_written") val modelWritten: Boolean = false,
    @SerialName("origin_count") val originCount: Int = 0,
    @SerialName("due_in") val dueIn: String? = null,
    /** `title` is not a name of this text — a section heading, or the note's name lent to it. */
    @SerialName("borrowed_name") val borrowedName: Boolean = false,
    /** Recalled beside another hit rather than ranked: it competed for nothing and has no rank. */
    val via: String? = null,
    val reason: String? = null,
    /** The ranking's own answer, in the server's words. Only with `explain`. */
    @SerialName("why_ranked") val whyRanked: String? = null,
    /** The next passage of the same document, where this one's goes on. */
    @SerialName("continues_to") val continuesTo: String? = null,
)

/**
 * A result list as the app's door answers it: the rows, the search they were
 * recorded under, and whether the reranker confirmed the order.
 */
@Serializable
data class SearchPage(
    val items: List<Hit> = emptyList(),
    val next: String? = null,
    /** What an open, a verdict and a gap name. Absent where searches are not recorded. */
    val event: String? = null,
    val reranked: Boolean = false,
)

@Serializable
data class CorpusRow(
    val id: String,
    val origin: String = "",
    val label: String = "",
    val named: Boolean = false,
    val status: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("source_url") val sourceUrl: String? = null,
    @SerialName("near_dupe_of") val nearDupeOf: String? = null,
)

@Serializable
data class Span(@SerialName("start_line") val startLine: Long, @SerialName("end_line") val endLine: Long)

@Serializable
data class Chunk(
    val id: String,
    @SerialName("corpus_id") val corpusId: String? = null,
    val provenance: String = "",
    val text: String = "",
    val title: String? = null,
    val category: String? = null,
    val tags: List<String> = emptyList(),
    val caveats: List<String> = emptyList(),
    val status: String = "active",
    @SerialName("superseded_by") val supersededBy: String? = null,
    @SerialName("corpus_span") val span: Span? = null,
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("last_verified_at") val lastVerifiedAt: Long? = null,
    /** For a synthesized artifact: the questions it was written for. */
    val cues: List<String> = emptyList(),
    /** Verification failures. Empty means every check passed. */
    val flags: List<String> = emptyList(),
    @SerialName("flag_detail") val flagDetail: String? = null,
) {
    /** What the server's `names_its_own_text` decides: a passage and a note carry a heading that is not theirs. */
    val named: Boolean get() = !title.isNullOrBlank() && provenance != "passage" && provenance != "note"
}

@Serializable
data class CorpusDetail(
    val id: String,
    val origin: String = "",
    @SerialName("title_hint") val title: String? = null,
    @SerialName("raw_text") val text: String = "",
    val status: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    @SerialName("source_url") val sourceUrl: String? = null,
    val chunks: List<Chunk> = emptyList(),
)

@Serializable
data class SourceRef(val id: String, val title: String? = null, val origin: String = "", @SerialName("source_url") val sourceUrl: String? = null)

/**
 * A neighbour, or something this artifact has been needed alongside. [why]
 * and [corpusTitle] are only on a `seen_together` row: a neighbour is near by
 * resemblance and needs no explaining.
 */
@Serializable
data class RelatedRow(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val snippet: String = "",
    val why: String? = null,
    @SerialName("corpus_title") val corpusTitle: String? = null,
)

/**
 * What the web pane lists beside an artifact, and where it continues. Two
 * lists and not one, because they answer different questions: what this
 * resembles, and what it has been reached for together with.
 */
@Serializable
data class Related(
    val related: List<RelatedRow> = emptyList(),
    @SerialName("seen_together") val seenTogether: List<RelatedRow> = emptyList(),
    /** The next passage of the same document, where this one stops mid-sentence. */
    @SerialName("continues_at") val continuesAt: String? = null,
) {
    val isEmpty: Boolean get() = related.isEmpty() && seenTogether.isEmpty() && continuesAt == null
}

/** One line of the source beside an artifact. [inSpan] is a line the artifact was drawn from, not context. */
@Serializable
data class SourceLine(val number: Long, val text: String = "", @SerialName("in_span") val inSpan: Boolean = false)

/**
 * The lines an artifact was drawn from, with a little context either side —
 * the source column of the web pane. [corpusId] is null for a merge, which
 * belongs to no document, and where the document is gone.
 */
@Serializable
data class SourceSlice(
    @SerialName("corpus_id") val corpusId: String? = null,
    val label: String = "",
    val lines: List<SourceLine> = emptyList(),
)

@Serializable
data class LastKept(val id: String, val label: String = "", val named: Boolean = false, val at: Long = 0)

@Serializable
data class Examples(val lang: String = "en", val remind: String = "", val journal: String = "")

/** What the base says about itself: which doors are open, and the idle line's facts. */
@Serializable
data class Status(
    /** Whether `POST /transcribe` is open: a speech model is configured. The mic is drawn only where it is. */
    val transcribe: Boolean = false,
    val asks: Boolean = false,
    val vision: Boolean = false,
    /** Searches and questions are recorded: the verdict bars and the gap button are drawn only then. */
    val learn: Boolean = false,
    val recommend: Boolean = false,
    val held: Held = Held(),
    @SerialName("last_kept") val lastKept: LastKept? = null,
    val examples: Examples = Examples(),
    /** The base is young: the idle column still teaches what a paste becomes. */
    val teach: Boolean = false,
)

/**
 * The server flattens the chunk into the top level and sets `source` beside
 * it; read twice, once for each. [searchEvent] is the search this open was
 * attributed to, where the read named one: the verdict bar is drawn only then.
 */
data class ArtifactDetail(val chunk: Chunk, val source: SourceRef?, val searchEvent: String? = null)

@Serializable
private data class Beside(val source: SourceRef? = null, @SerialName("search_event") val searchEvent: String? = null)

fun decodeArtifact(body: String): ArtifactDetail {
    val b = ApiJson.decodeFromString(Beside.serializer(), body)
    return ArtifactDetail(ApiJson.decodeFromString(Chunk.serializer(), body), b.source, b.searchEvent)
}

/** The lines of the pane that are about the artifact rather than of it. */
@Serializable
data class About(
    val tag: String? = null,
    val probes: List<String> = emptyList(),
    /** The open condensation's action id, where the live text is a condensed one. */
    val condensed: String? = null,
    @SerialName("due_in") val dueIn: String? = null,
)

/** One band of a source: a stretch of lines beside what was written from them, or a red one saying nothing was. */
@Serializable
data class Band(
    val from: Long,
    val to: Long,
    val gap: Boolean = false,
    /** `reads lines 118–141`, where a re-read is offered. */
    val reread: String? = null,
    val lines: List<SourceLine> = emptyList(),
    @SerialName("artifact_ids") val artifactIds: List<String> = emptyList(),
    /** Artifacts carded in an earlier band that also span this one. */
    val echoes: List<String> = emptyList(),
)

@Serializable
data class Promoted(val idx: Long, val from: Long, val to: Long)

/** The corpus page as data: what stands above the bands, and the bands. */
@Serializable
data class CorpusPage(
    val image: Boolean = false,
    val pdf: Boolean = false,
    val unread: Boolean = false,
    val restored: Boolean = false,
    val note: String? = null,
    val coverage: String? = null,
    val meta: List<List<String>> = emptyList(),
    val exif: List<List<String>> = emptyList(),
    val promoted: List<Promoted> = emptyList(),
    val bands: List<Band> = emptyList(),
    val unplaced: List<String> = emptyList(),
    @SerialName("written_from") val writtenFrom: List<String> = emptyList(),
)

/** What capture will do with the box, said before it is pressed. Empty [kind] for an empty box. */
@Serializable
data class Echo(val kind: String = "", val detail: String = "")

@Serializable
data class FacetCount(val value: String, val count: Long = 0)

@Serializable
data class Facets(val categories: List<FacetCount> = emptyList())

@Serializable
data class Recorded(val captured: Long = 0, val pending: Long = 0, val judged: Long = 0)

@Serializable
data class AskedRecorded(val asked: Long = 0, val judged: Long = 0)

/** What is being recorded. Both null while `[learn]` is off. */
@Serializable
data class Feedback(val searches: Recorded? = null, val asks: AskedRecorded? = null)

@Serializable
data class LangRow(val value: String, val label: String)

@Serializable
data class LangSetting(val chosen: String = "", val langs: List<LangRow> = emptyList())

@Serializable
data class NotifySetting(
    @SerialName("gotify_url") val gotifyUrl: String = "",
    @SerialName("gotify_token_set") val gotifyTokenSet: Boolean = false,
    @SerialName("up_endpoint") val upEndpoint: String = "",
    @SerialName("up_device") val upDevice: String? = null,
    @SerialName("up_legacy") val upLegacy: Boolean = false,
)

@Serializable
data class LinkCounts(val total: Long = 0, val related: Long = 0, @SerialName("judge_queue") val judgeQueue: Long = 0)

@Serializable
data class SweepCount(val n: Long = 0, val what: String = "")

@Serializable
data class SweepRun(
    val `when`: String = "",
    val stage: String = "",
    @SerialName("stage_id") val stageId: String = "",
    val error: String = "",
    val took: String = "",
    val counts: List<SweepCount> = emptyList(),
)

@Serializable
data class OfferRate(val rung: String = "", val shown: Long = 0, val opened: Long = 0)

@Serializable
data class Retrying(
    val stage: String = "",
    @SerialName("target_id") val targetId: String = "",
    val attempts: Long = 0,
    val due: String = "",
    @SerialName("last_error") val lastError: String = "",
)

/** What the machine is doing: the disclosure at the foot of Insights. */
@Serializable
data class Machine(
    val artifacts: Long = 0,
    val vectors: Long = 0,
    /** `[["pending", 2], …]`: a job state and its count. */
    val jobs: List<List<kotlinx.serialization.json.JsonPrimitive>> = emptyList(),
    @SerialName("oldest_pending_secs") val oldestPendingSecs: Long? = null,
    val links: LinkCounts? = null,
    @SerialName("last_day") val lastDay: List<SweepCount> = emptyList(),
    @SerialName("last_day_failures") val lastDayFailures: Long = 0,
    @SerialName("sweep_history") val sweepHistory: List<SweepRun> = emptyList(),
    @SerialName("offer_rates") val offerRates: List<OfferRate> = emptyList(),
    val retrying: List<Retrying> = emptyList(),
)

@Serializable
data class Sleep(
    val runs: List<String> = emptyList(),
    @SerialName("idle_mins") val idleMins: Long = 0,
    @SerialName("unrehearsed_count") val unrehearsedCount: Long = 0,
    /** `[[id, title], …]`. */
    val unrehearsed: List<List<String>> = emptyList(),
)

@Serializable
data class Evolve(
    val suspended: String? = null,
    val mode: String = "",
    val live: String = "",
    val params: String = "",
    val standing: String = "",
    val rehearsed: String = "",
    val history: List<String> = emptyList(),
    val actions: List<String> = emptyList(),
    val rules: String? = null,
)

/** What the base did on its own, in the sentences Insights says. */
@Serializable
data class Report(
    val sleep: Sleep? = null,
    val evolve: Evolve? = null,
    /** `[recent, unsatisfied]`, or null while `[learn]` is off. */
    val pursuits: List<Long>? = null,
    @SerialName("more_pairs") val morePairs: Long = 0,
)

/** What the web's `_ask_kept.html` says became of a kept answer. */
@Serializable
data class Kept(val id: String, val duplicate: Boolean = false, val parked: Boolean = false, @SerialName("near_dupe_percent") val nearDupePercent: Long = 0)

@Serializable
data class VerdictAnswer(val state: String = "", val already: Boolean = false)

@Serializable
data class GapAnswer(val recorded: Boolean = false)

@Serializable
data class AskVerdictAnswer(val verdict: String? = null)

@Serializable
data class CarriedAnswer(val carried: Boolean = false, val verdict: String? = null)

@Serializable
data class NotAReminderAnswer(val undo: String? = null)

@Serializable
data class NotifyTested(val sent: Boolean = false, val error: String? = null)

@Serializable
data class DayEntryAnswer(val id: String)

@Serializable
data class NodeSource(
    @SerialName("corpus_id") val corpusId: String,
    val label: String = "",
    @SerialName("start_line") val startLine: Long? = null,
    @SerialName("end_line") val endLine: Long? = null,
)

@Serializable
data class Node(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val kind: String = "",
    @SerialName("created_at") val createdAt: Long = 0,
    val source: NodeSource? = null,
    val replaced: Boolean = false,
    val missing: Boolean = false,
    val children: List<Node> = emptyList(),
)

@Serializable
data class Lineage(
    val roots: List<Node> = emptyList(),
    @SerialName("also_replaced") val alsoReplaced: List<Node> = emptyList(),
    /** The walk stopped early. Said on the screen: a tree that quietly stops reads as a whole history. */
    val truncated: Boolean = false,
) {
    val isEmpty: Boolean get() = roots.isEmpty() && alsoReplaced.isEmpty()
}

@Serializable
data class Version(
    val n: Long,
    val title: String? = null,
    val text: String = "",
    val caveats: List<String> = emptyList(),
    @SerialName("created_at") val createdAt: Long = 0,
)

@Serializable
data class DayCorpus(val id: String, val label: String = "", val named: Boolean = false, val at: Long = 0, val text: String = "")

@Serializable
data class DayMoment(
    val id: String,
    @SerialName("artifact_id") val artifactId: String,
    val label: String = "",
    val named: Boolean = false,
    val kind: String = "due",
    val at: Long? = null,
    val done: Boolean = false,
    val span: String? = null,
)

@Serializable
data class Opened(val id: String, val label: String = "", val named: Boolean = false)

@Serializable
data class DaySitting(
    @SerialName("opened_at") val openedAt: Long,
    @SerialName("closed_at") val closedAt: Long,
    val query: String = "",
    val searches: Int = 0,
    val opened: List<Opened> = emptyList(),
)

@Serializable
data class Day(
    val date: String,
    val tz: String = "UTC",
    val entries: List<DayCorpus> = emptyList(),
    val captured: List<DayCorpus> = emptyList(),
    @SerialName("was_due") val wasDue: List<DayMoment> = emptyList(),
    val refers: List<DayMoment> = emptyList(),
    val sittings: List<DaySitting> = emptyList(),
) {
    val isEmpty: Boolean get() = entries.isEmpty() && captured.isEmpty() && wasDue.isEmpty() && refers.isEmpty() && sittings.isEmpty()
}

@Serializable
data class Moment(
    val id: String,
    @SerialName("artifact_id") val artifactId: String,
    val at: Long? = null,
    val rule: String? = null,
    @SerialName("snoozed_until") val snoozedUntil: Long? = null,
    /** `set` by a person, or read out of the note by the stage. */
    val source: String = "set",
    val kind: String = "due",
    val span: String? = null,
)

@Serializable
data class DueRow(val moment: Moment, val title: String = "", val named: Boolean = false, val opening: String = "") {
    /** When it is due as the list orders it: a snooze moves it. */
    val at: Long? get() = moment.snoozedUntil ?: moment.at
}

@Serializable
data class Offer(
    @SerialName("artifact_id") val artifactId: String,
    val label: String = "",
    val named: Boolean = false,
    val snippet: String = "",
    /** The ladder's word — pattern, similar, tentative, random. Sent back verbatim by `seen`. */
    val rung: String = "random",
    val slot: Long? = null,
    val events: Long = 0,
    val at: Long? = null,
    @SerialName("at_tz") val atTz: String? = null,
)

@Serializable
data class OfferAnswer(val offer: Offer? = null)

/** What an ask ends with: the answer the server stands behind, and what it could not vouch for. */
@Serializable
data class AskAnswer(
    val answer: String = "",
    val citations: List<Hit> = emptyList(),
    val dropped: Int = 0,
    val truncated: Boolean = false,
    val abstained: Boolean = false,
    /** Literals in the answer that no excerpt carries: the model's own, not the base's. */
    val unsupported: List<String> = emptyList(),
    @SerialName("retired_only") val retiredOnly: Boolean = false,
    /** The question as recorded, where it was: what a verdict, a carried excerpt and a keep name. */
    @SerialName("event_id") val eventId: String? = null,
)

// ── Judging ──────────────────────────────────────────────────────────────────

/** One side of a duplicate pair, with enough of it to decide by. */
@Serializable
data class PairSide(
    val id: String,
    val label: String = "",
    val named: Boolean = false,
    val excerpt: String = "",
)

/**
 * One open pair, carrying only what somebody actually established about it.
 *
 * Three of these fields are there to stop a card claiming more than was found.
 * [unjudged] means the sweep filed this on a cosine score and nothing has read
 * it since, so "these two cover the same ground" is a finding nobody made —
 * the measurement is what there is to draw. [viaLink] means no cosine was ever
 * computed, so [percent] is not a similarity and must not be shown as one.
 * [mergeable] is whether the merge path would take a synthesis at all.
 *
 * Its default is false, and deliberately: a server that does not send the
 * field leaves the button out, rather than offering a press that can only come
 * back a validation error.
 */
@Serializable
data class Pair(
    val id: Long,
    val percent: Long = 0,
    @SerialName("via_link") val viaLink: Boolean = false,
    val a: PairSide,
    val b: PairSide,
    /** The judge's line, where one was written. */
    val finding: String? = null,
    val contradiction: Boolean = false,
    val vacuous: Boolean = false,
    val unjudged: Boolean = false,
    val unmergeable: Boolean = false,
    val mergeable: Boolean = false,
    @SerialName("synthesis_asked") val synthesisAsked: Boolean = false,
    /** The artifact the judge's proposal amounts to keeping, where it made one. */
    val keeps: String? = null,
)

/** One decision, however many pairs it takes to state it. */
@Serializable
data class PairCluster(val members: Int = 1, val pairs: List<Pair> = emptyList())

/**
 * The pair queue, and how many are waiting beyond it.
 *
 * Its own type rather than [Page], for the reason [SetAside] has one: the
 * queue is bounded rather than paged — `next` is always null — so the cap is
 * the only thing that can say there is more, and a cap that goes unreported
 * reads as the whole queue. [more] is the number, which is what the web page
 * says out loud; zero where nothing is waiting, and where a server too old to
 * send it said nothing.
 */
@Serializable
data class PairQueue(
    val items: List<PairCluster> = emptyList(),
    val next: String? = null,
    val more: Int = 0,
)

/** One question nothing covered. [kind] and [id] are what dismissing it names. */
@Serializable
data class GapMember(val kind: String = "", val id: String = "", val text: String = "")

/**
 * Questions the sweep found to be about one subject. [labelledBy] is `model`
 * or `terms` — whether a model named this group or its shared wording did,
 * which is the difference between a reading and a description.
 */
@Serializable
data class GapCluster(
    val label: String = "",
    @SerialName("labelled_by") val labelledBy: String = "",
    val members: List<GapMember> = emptyList(),
)

@Serializable
data class Held(val corpora: Long = 0, val artifacts: Long = 0, val segments: Long = 0, val synthesized: Long = 0)

@Serializable
data class UsedBand(val label: String = "", val count: Long = 0)

/** Null throughout where nothing was judged — never 0.00, which reads as a score. */
@Serializable
data class Retrieval(
    val judged: Long = 0,
    @SerialName("recall_at_10") val recallAt10: Double? = null,
    val mrr: Double? = null,
)

/** What the base is like. Read-only: nothing about tuning crosses. */
@Serializable
data class Insights(
    val held: Held = Held(),
    val used: List<UsedBand> = emptyList(),
    /** Absent where no searches are recorded. */
    val retrieval: Retrieval? = null,
)

/** What the base put beside a set-aside row: a merge's sources, a near-duplicate's winner. */
@Serializable
data class Beside(
    val id: String = "",
    @SerialName("corpus_id") val corpusId: String = "",
    val label: String = "",
    val named: Boolean = false,
)

/**
 * One thing the base did on its own, or is waiting to be told about.
 *
 * [kind] is the whole of what says which answers the row admits — see
 * [actionsFor]. [subjectId] is what those answers name, and which thing that
 * is depends on the kind: a corpus for `parked`, the artifact for the rest.
 */
@Serializable
data class SetAsideRow(
    val kind: String = "",
    @SerialName("subject_id") val subjectId: String = "",
    /** The artifact to open. Null for a parked capture, which is a corpus. */
    @SerialName("artifact_id") val artifactId: String? = null,
    val label: String = "",
    val named: Boolean = false,
    /** What tells two rows with one label apart. Empty where nothing does. */
    val subtitle: String = "",
    val why: String = "",
    val beside: List<Beside> = emptyList(),
    /** The one thing about this row that is not simply reversible. */
    val caveat: String? = null,
)

/** The undo list, and whether any cap bit. */
@Serializable
data class SetAside(
    val items: List<SetAsideRow> = emptyList(),
    val next: String? = null,
    val capped: Boolean = false,
)

/** An answer a set-aside row admits. Each is a route; none of them is a rendering. */
enum class SetAsideAction { Verify, Deprecate, Reactivate, UndoMerge, ResolveReplace, ResolveKeepBoth, ResolveDiscard }

/**
 * Which answers a row admits, from its `kind` and nothing else — no reading of
 * its wording, no guess from what is beside it. A kind this build has never
 * heard of admits none, which is what lets the server grow a seventh without
 * breaking an older app.
 *
 * `hidden` arrives from two places — an artifact superseded by a near-duplicate
 * and one deprecated by hand — and the kind does not say which. It does not
 * have to: `reactivate` answers either, routing a supersession through the
 * server's own unsupersede, so the one button here can never be a press that
 * comes back refused.
 */
fun actionsFor(kind: String): List<SetAsideAction> = when (kind) {
    "unverified" -> listOf(SetAsideAction.Verify, SetAsideAction.Deprecate)
    "merged" -> listOf(SetAsideAction.UndoMerge)
    "generated" -> listOf(SetAsideAction.Deprecate)
    "hidden", "buried" -> listOf(SetAsideAction.Reactivate)
    "parked" -> listOf(SetAsideAction.ResolveReplace, SetAsideAction.ResolveKeepBoth, SetAsideAction.ResolveDiscard)
    else -> emptyList()
}

/** How each body is read. */
object Decode {
    private fun <T> page(item: kotlinx.serialization.KSerializer<T>): (String) -> Page<T> =
        { ApiJson.decodeFromString(Page.serializer(item), it) }

    val hits = page(Hit.serializer())
    val search: (String) -> SearchPage = { ApiJson.decodeFromString(SearchPage.serializer(), it) }
    val about: (String) -> About = { ApiJson.decodeFromString(About.serializer(), it) }
    val bands: (String) -> CorpusPage = { ApiJson.decodeFromString(CorpusPage.serializer(), it) }
    val facets: (String) -> Facets = { ApiJson.decodeFromString(Facets.serializer(), it) }
    val echo: (String) -> Echo = { ApiJson.decodeFromString(Echo.serializer(), it) }
    val feedback: (String) -> Feedback = { ApiJson.decodeFromString(Feedback.serializer(), it) }
    val lang: (String) -> LangSetting = { ApiJson.decodeFromString(LangSetting.serializer(), it) }
    val notify: (String) -> NotifySetting = { ApiJson.decodeFromString(NotifySetting.serializer(), it) }
    val machine: (String) -> Machine = { ApiJson.decodeFromString(Machine.serializer(), it) }
    val report: (String) -> Report = { ApiJson.decodeFromString(Report.serializer(), it) }
    val kept: (String) -> Kept = { ApiJson.decodeFromString(Kept.serializer(), it) }
    val verdict: (String) -> VerdictAnswer = { ApiJson.decodeFromString(VerdictAnswer.serializer(), it) }
    val gap: (String) -> GapAnswer = { ApiJson.decodeFromString(GapAnswer.serializer(), it) }
    val askVerdict: (String) -> AskVerdictAnswer = { ApiJson.decodeFromString(AskVerdictAnswer.serializer(), it) }
    val carried: (String) -> CarriedAnswer = { ApiJson.decodeFromString(CarriedAnswer.serializer(), it) }
    val notAReminder: (String) -> NotAReminderAnswer = { ApiJson.decodeFromString(NotAReminderAnswer.serializer(), it) }
    val notifyTested: (String) -> NotifyTested = { ApiJson.decodeFromString(NotifyTested.serializer(), it) }
    val dayEntry: (String) -> DayEntryAnswer = { ApiJson.decodeFromString(DayEntryAnswer.serializer(), it) }
    /** For a write that answers `204`, or whose body is not worth reading. */
    val nothing: (String) -> Unit = { }
    val moments = page(Moment.serializer())
    val corpora = page(CorpusRow.serializer())
    val versions = page(Version.serializer())
    val due = page(DueRow.serializer())
    val corpus: (String) -> CorpusDetail = { ApiJson.decodeFromString(CorpusDetail.serializer(), it) }
    val lineage: (String) -> Lineage = { ApiJson.decodeFromString(Lineage.serializer(), it) }
    val day: (String) -> Day = { ApiJson.decodeFromString(Day.serializer(), it) }
    val offer: (String) -> OfferAnswer = { ApiJson.decodeFromString(OfferAnswer.serializer(), it) }
    val artifact: (String) -> ArtifactDetail = ::decodeArtifact
    val related: (String) -> Related = { ApiJson.decodeFromString(Related.serializer(), it) }
    val source: (String) -> SourceSlice = { ApiJson.decodeFromString(SourceSlice.serializer(), it) }
    val status: (String) -> Status = { ApiJson.decodeFromString(Status.serializer(), it) }
    val pairs: (String) -> PairQueue = { ApiJson.decodeFromString(PairQueue.serializer(), it) }
    val gaps = page(GapCluster.serializer())
    val insights: (String) -> Insights = { ApiJson.decodeFromString(Insights.serializer(), it) }
    val setAside: (String) -> SetAside = { ApiJson.decodeFromString(SetAside.serializer(), it) }
}

/** The reads, by name. One place knows a path; a screen knows what it wants. */
object Api {
    /**
     * A search from the app's door: recorded under the person, like the web's,
     * and answered with the event it was recorded under. The first pass is a
     * typing pass, at vector-order speed; [refine] is the second, reranked
     * one the web makes once the typing has settled, and it explains itself.
     */
    fun search(q: String, category: String? = null, refine: Boolean = false) = Request(
        "/api/v1/search",
        mapOf(
            "q" to q,
            "door" to "app",
            "category" to category?.takeIf { it.isNotEmpty() },
            "rerank" to if (refine) "true" else null,
            "explain" to if (refine) "1" else null,
        ),
    )
    fun facets() = Request("/api/v1/facets")
    fun echo(q: String) = Request("/api/v1/echo", mapOf("q" to q))
    fun about(id: String) = Request("/api/v1/artifacts/$id/about")
    fun bands(id: String) = Request("/api/v1/corpora/$id/bands")
    fun feedback() = Request("/api/v1/feedback")
    fun lang() = Request("/api/v1/settings/lang")
    fun notify() = Request("/api/v1/settings/notify")
    fun machine() = Request("/api/v1/insights/machine")
    fun report() = Request("/api/v1/insights/report")
    /** Dates that refer to a window: what is coming up. */
    fun events(from: Long, to: Long) = Request("/api/v1/moments", mapOf("kind" to "event", "from" to from.toString(), "to" to to.toString()))
    fun resurface() = Request("/api/v1/resurface", mapOf("limit" to "5"))
    fun due() = Request("/api/v1/moments", mapOf("kind" to "due"))
    fun corpora(after: String?) = Request("/api/v1/corpora", mapOf("limit" to "50", "after" to after))
    fun corpus(id: String) = Request("/api/v1/corpora/$id")
    /** [event] is the search that listed it, so the open is attributed to that search and the bar is drawn. */
    fun artifact(id: String, event: String? = null) = Request("/api/v1/artifacts/$id", mapOf("event" to event))
    fun lineage(id: String) = Request("/api/v1/artifacts/$id/lineage")
    fun versions(id: String) = Request("/api/v1/artifacts/$id/versions")
    fun related(id: String) = Request("/api/v1/artifacts/$id/related")
    fun source(id: String) = Request("/api/v1/artifacts/$id/source")
    fun status() = Request("/api/v1/status")
    fun day(date: String, tz: String) = Request("/api/v1/days/$date", mapOf("tz" to tz))
    fun pairs() = Request("/api/v1/pairs")
    fun gaps() = Request("/api/v1/gaps")
    fun insights() = Request("/api/v1/insights")
    fun setAside() = Request("/api/v1/insights/set-aside")

    const val CONTEXT = "/api/v1/context"
    const val SEEN = "/api/v1/context/seen"

    // The immediate writes: pressed where the server is, answered in words.
    fun searchVerdict(event: String) = "/api/v1/search/$event/verdict"
    fun searchGap(event: String) = "/api/v1/search/$event/gap"
    fun askVerdict(event: String) = "/api/v1/asks/$event/verdict"
    fun askCarried(event: String) = "/api/v1/asks/$event/carried"
    fun askKeep(event: String) = "/api/v1/asks/$event/keep"
    fun notAReminder(momentId: String) = "/api/v1/moments/$momentId/not-a-reminder"
    fun dayEntry(date: String) = "/api/v1/days/$date/entry"
    const val FEEDBACK = "/api/v1/feedback"
    const val LANG = "/api/v1/settings/lang"
    const val NOTIFY = "/api/v1/settings/notify"
    const val NOTIFY_TEST = "/api/v1/settings/notify/test"
    fun artifactEdit(id: String) = "/api/v1/artifacts/$id"
    fun dwell(id: String) = "/api/v1/artifacts/$id/dwell"

    // The owed writes: outbox rows, delivered when the server can be reached.
    fun momentDate(id: String) = "/api/v1/moments/$id/date"
    fun momentUndone(id: String) = "/api/v1/moments/$id/undone"
    fun momentUnsnooze(id: String) = "/api/v1/moments/$id/unsnooze"
    fun isAReminder(artifactId: String) = "/api/v1/artifacts/$artifactId/is-a-reminder"
    fun reviewed(id: String) = "/api/v1/artifacts/$id/reviewed"
    fun dismissLink(id: String, other: String) = "/api/v1/artifacts/$id/links/$other/dismiss"
    fun condensationUndo(action: String) = "/api/v1/condensations/$action/undo"
    fun corpusDelete(id: String) = "/api/v1/corpora/$id"
    fun corpusReprocess(id: String) = "/api/v1/corpora/$id/reprocess"
    fun corpusReread(id: String) = "/api/v1/corpora/$id/reread"
    fun corpusEntry(id: String) = "/api/v1/corpora/$id/entry"
    fun unpromote(id: String, idx: Long) = "/api/v1/corpora/$id/segments/$idx/unpromote"

    fun json(vararg pairs: kotlin.Pair<String, Any?>): String = JsonObject(
        pairs.filter { it.second != null }.associate { (k, v) ->
            k to when (v) {
                is String -> kotlinx.serialization.json.JsonPrimitive(v)
                is Boolean -> kotlinx.serialization.json.JsonPrimitive(v)
                is Number -> kotlinx.serialization.json.JsonPrimitive(v)
                else -> kotlinx.serialization.json.JsonPrimitive(v.toString())
            }
        },
    ).toString()
    fun seen(o: Offer): String = JsonObject(
        buildMap {
            put("artifact_id", kotlinx.serialization.json.JsonPrimitive(o.artifactId))
            put("rung", kotlinx.serialization.json.JsonPrimitive(o.rung))
            if (o.slot != null) put("slot", kotlinx.serialization.json.JsonPrimitive(o.slot))
        },
    ).toString()
}
