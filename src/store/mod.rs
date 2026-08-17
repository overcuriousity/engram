pub mod artifacts;
pub mod asks;
pub mod attachments;
pub mod auth;
pub mod corpora;
pub mod feedback;
pub mod gaps;
pub mod jobs;
pub mod lineage;
pub mod links;
pub mod pairs;
pub mod segments;
pub mod shingle;

use crate::config::StoreConfig;
use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
    /// Held for the length of a capture write. `record_search` reads the
    /// previous event and then writes over it, and the UI fires one of these per
    /// keystroke: two overlapping transactions upgrade from read to write on the
    /// same snapshot, which SQLite answers with `SQLITE_BUSY_SNAPSHOT` and no
    /// `busy_timeout` can wait out. Shared by every clone, which is what makes
    /// it a queue rather than eight of them.
    capture: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Store {
    pub async fn connect(cfg: &StoreConfig) -> Result<Store> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cfg.path))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(true)
            // WAL lets readers proceed during writes, which matters because the
            // worker pool writes constantly while the UI reads.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Bring the database up to the schema this binary expects.
    ///
    /// One statement of what the schema *is*, rather than a chain of diffs
    /// describing how it came to be. Every object is `IF NOT EXISTS`, so this
    /// creates what is missing on a fresh database and is a no-op on one that
    /// already has it — then `ADDED_COLUMNS` appends the few columns that
    /// arrived after a table was already deployed, since `IF NOT EXISTS` cannot.
    ///
    /// It still checks afterwards. A table that already exists is otherwise left
    /// as it is, columns and all, so a database from before a column was added
    /// would survive this call and fail much later, in a request, with a bare
    /// `ColumnNotFound` panic that says nothing about the real cause.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("schema.sql");

        // Columns that arrived after their table was already deployed.
        //
        // `schema.sql` says what the schema *is*, and `CREATE TABLE IF NOT
        // EXISTS` is a no-op against a table that is already there — so a column
        // added to that file never reaches a running base, and the check below
        // then refuses to start, correctly, saying the database is older than
        // the schema. That is the right answer while nothing is deployed and the
        // wrong one afterwards: it asks an operator to recreate a knowledge base
        // to gain a column with a default.
        //
        // So each such column is named here once. SQLite's `ALTER TABLE` can
        // only append, and only with a default, which is exactly the shape of
        // every entry this list is allowed to hold. Adding one is safe; changing
        // or reordering one is not. A column needing more than an append does
        // not belong here — it belongs in a recreate.
        const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
            ("jobs", "seq", "INTEGER NOT NULL DEFAULT 0"),
            (
                "artifact_pairs",
                "judge_unreadable",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            // Nullable, and rightly so: a corpus captured before the extension
            // and link doors existed was read from nowhere this can name.
            ("corpora", "source_url", "TEXT"),
            // The three that arrived with merging. Every artifact predating it
            // was captured, from a corpus, with nothing outstanding — which is
            // what each default says, so the append needs no backfill.
            (
                "artifacts",
                "provenance",
                "TEXT NOT NULL DEFAULT 'captured'",
            ),
            ("artifacts", "source_count", "INTEGER NOT NULL DEFAULT 0"),
            ("artifacts", "lifecycle_dirty", "INTEGER NOT NULL DEFAULT 0"),
            // Arrived with the stranded-merge reap. NULL on every pair
            // predating it, which is correct: those settlements predate the
            // column and are not reopenable by merge id.
            ("artifact_pairs", "merged_into", "TEXT"),
            // The operator's partial restore of a merge source. 0 on every
            // existing row, which is correct: nothing predating the column
            // was explicitly restored.
            ("artifact_sources", "restored", "INTEGER NOT NULL DEFAULT 0"),
            // Arrived with image capture. Every corpus predating it recorded
            // nothing beyond its text, which is what the empty object says.
            ("corpora", "metadata", "TEXT NOT NULL DEFAULT '{}'"),
            // Arrived with associative memory. Both have defaults that are the
            // truth about an artifact captured before it existed — full
            // accessibility, no stamp — and the stamp is backfilled below from
            // `created_at`, which is when it was in fact last activated.
            ("artifacts", "activation", "REAL NOT NULL DEFAULT 1.0"),
            ("artifacts", "activated_at", "INTEGER NOT NULL DEFAULT 0"),
            // Arrived with knowledge gaps. NULL on every existing row: nothing
            // predating it was covered.
            ("search_events", "dismissed_at", "INTEGER"),
            // Arrived with the category fold. NULL on every row predating it,
            // which is what makes the payload repair finite: it stamps what it
            // rewrites and empties itself.
            ("artifacts", "payload_synced_at", "INTEGER"),
            // Arrived with the settings page. NULL on every token minted
            // before it, which is the truth: nothing recorded what asked.
            ("api_tokens", "user_agent", "TEXT"),
        ];

        // Before the schema, not after. `schema.sql` builds an index over `seq`,
        // and an index cannot name a column that is not there yet — applying the
        // file first fails on exactly the databases this list exists to rescue.
        // On a fresh one the table does not exist yet, the loop skips, and the
        // schema creates it with the column already in place.
        for (table, column, decl) in ADDED_COLUMNS {
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            // An empty list means the table does not exist yet, in which case
            // `schema.sql` just created it with the column already in place.
            if have.is_empty() || have.iter().any(|h| h.eq_ignore_ascii_case(column)) {
                continue;
            }
            // A table name cannot be a bind parameter, so this statement has to
            // be built as a string, and sqlx rightly demands the assertion be
            // made explicitly. The audit it asks for: all three parts come from
            // `ADDED_COLUMNS` above, which is a compile-time constant in this
            // file. No caller, request or database value reaches it.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {table} ADD COLUMN {column} {decl}"
            )))
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            tracing::info!(table, column, "added a column to an existing database");
        }

        // The one change merging needed that an append cannot make. `corpus_id`
        // went from NOT NULL to nullable because a merged artifact belongs to no
        // single corpus, and SQLite can add a column but not relax a constraint
        // on one that is already there. Left unsaid, such a database passes every
        // check here and then fails at the first merge, on a NOT NULL constraint,
        // a long way from the cause. So it is said here, and it names the cost:
        // this is the case `ADDED_COLUMNS` deliberately does not cover.
        let strict_corpus: Option<i64> = sqlx::query_scalar(
            r#"SELECT "notnull" FROM pragma_table_info('artifacts') WHERE name = 'corpus_id'"#,
        )
        .fetch_optional(&self.pool)
        .await?;
        if strict_corpus == Some(1) {
            return Err(crate::error::Error::Store(
                "this database predates merging: artifacts.corpus_id is still NOT NULL, \
                 and a merged artifact belongs to no corpus. SQLite cannot relax that \
                 constraint in place, so the artifacts table has to be rebuilt — copy it \
                 into one declared by the current schema, or recreate the database."
                    .into(),
            ));
        }

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;

        // The judge became the dedupe unit and its stage name went with it. A
        // leftover row would otherwise be claimed under `Stage::parse`'s
        // Synthesize fallback and aimed at a pair id. Deleted rather than
        // renamed: the pair is still pending, so the next sweep re-arms it
        // under its real stage, and a rename could collide with a dedupe row
        // the sweep has already written for the same pair.
        sqlx::query("DELETE FROM jobs WHERE stage = 'judge'")
            .execute(&self.pool)
            .await?;
        // Verdicts recorded while acting was switched off. There is no such
        // switch now, so they go back to the judge and are acted on.
        sqlx::query("UPDATE artifact_pairs SET state = 'pending' WHERE state = 'would_merge'")
            .execute(&self.pool)
            .await?;
        // The kind became a closed vocabulary of form words. Rows written while
        // it was a free string hold subject words — "System Administration",
        // "Forensic Science / Criminalistics" — and the search page builds its
        // filter row from whatever is stored, so closing the schema alone would
        // change nothing an operator can see.
        //
        // Folded rather than dropped: `other` is true of them, and the text,
        // the title and the vector are untouched. Idempotent like everything
        // else here — a second run matches nothing.
        //
        // The `AssertSqlSafe` audit: every value interpolated comes from
        // `CATEGORIES`, a compile-time constant. No caller, request or database
        // value reaches this string.
        let listed = crate::infer::prompt::CATEGORIES
            .iter()
            .map(|c| format!("'{c}'"))
            .collect::<Vec<_>>()
            .join(",");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "UPDATE artifacts SET category = 'other'
             WHERE category IS NOT NULL AND category NOT IN ({listed})"
        )))
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        // The one default an append cannot state: `activated_at` has to be the
        // artifact's own creation time, not zero. Left at zero every artifact
        // predating this column reads as decayed to nothing since 1970 — the
        // whole base equally inaccessible, which is the opposite of the truth.
        //
        // Keyed on a zeroed stamp rather than on "this call ran the ALTER",
        // because those are not the same moment. The `corpus_id` check above
        // returns `Err` after both ALTERs have committed, and the operator it
        // sends off to rebuild the artifacts table comes back to a database
        // whose columns already exist — a flag set by the ALTER would be false
        // from then on, and these fixups would never run at all. A kill inside
        // the same window does the same thing. A zeroed stamp is what actually
        // needs fixing, it is what the fix removes, and asking costs one scan
        // of `artifacts` at boot on a database that no longer has any.
        let adopting: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifacts WHERE activated_at = 0)")
                .fetch_one(&self.pool)
                .await?;
        if adopting {
            // One transaction, because the watermarks below are keyed on the
            // same zeroed stamp this backfill erases: committed separately, a
            // crash between them would leave nothing left to notice that the
            // second half never happened.
            let mut tx = self.pool.begin().await?;
            sqlx::query("UPDATE artifacts SET activated_at = created_at WHERE activated_at = 0")
                .execute(&mut *tx)
                .await?;
            // ...and the same moment decides where the association sweep starts
            // reading. Absent watermarks mean "from the epoch", which on a base
            // that has been recording searches for months would fold the entire
            // historical log in on the first tick — thousands of pairs bound at
            // once, every one of them stamped with the sweep's clock and so
            // undecayed, feeding priming and the judge queue from a past nobody
            // asked to relive. Learning starts now, from what happens next.
            //
            // A database with no artifacts at all is not adopting anything and
            // never reaches here, so a fresh install keeps both watermarks
            // absent and replays its own short log from the epoch, which is
            // both correct and free.
            //
            // The keys are `jobs::associate::{EVENTS_AFTER, JUDGED_AFTER}`,
            // spelled out because nothing else in `store` reaches up into
            // `jobs`; `watermarks_named_here_are_the_ones_the_sweep_reads`
            // pins them together.
            let at = now().to_string();
            for key in ["associate.events_after", "associate.judged_after"] {
                sqlx::query("INSERT OR IGNORE INTO meta (key, value) VALUES (?, ?)")
                    .bind(key)
                    .bind(&at)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            tracing::info!(
                "adopting associative memory: activation backfilled, \
                 the search log before now is left unread"
            );
        }

        let mut missing = Vec::new();
        for (table, columns) in schema_columns(SCHEMA) {
            // The table-valued form of `PRAGMA table_info`, which takes a bind
            // parameter where the pragma statement would need the name spliced
            // into the SQL.
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(&table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            for c in columns {
                if !have.iter().any(|h| h.eq_ignore_ascii_case(&c)) {
                    missing.push(format!("{table}.{c}"));
                }
            }
        }
        if !missing.is_empty() {
            return Err(crate::error::Error::Store(format!(
                "this database is older than the schema: {} missing. \
                 Recreate it, or add the columns by hand.",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// Fresh in-memory database with the schema applied, for the tests.
    pub async fn memory() -> Result<Store> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .foreign_keys(true);
        // One connection: every `sqlite::memory:` connection is a separate
        // database, so a multi-connection pool would see different data per query.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }
}

/// The `(table, columns)` pairs `schema.sql` declares.
///
/// Read out of the file rather than listed by hand, so it cannot fall behind
/// the schema it is checking — a hand-kept list of "columns added recently" is
/// exactly the thing everyone forgets to append to. The file is ours and one
/// column per line, which is what makes this much parsing enough: a line inside
/// a `CREATE TABLE` block starts with the column's name, unless it starts with a
/// comment or a table constraint.
fn schema_columns(sql: &str) -> Vec<(String, Vec<String>)> {
    const CONSTRAINTS: [&str; 5] = ["PRIMARY", "FOREIGN", "UNIQUE", "CHECK", "CONSTRAINT"];
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let mut open: Option<(String, Vec<String>)> = None;

    for line in sql.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            let name = rest.trim_end_matches('(').trim();
            open = Some((name.to_string(), Vec::new()));
            continue;
        }
        let Some((_, columns)) = open.as_mut() else {
            continue;
        };
        if line.starts_with(')') {
            tables.push(open.take().expect("just borrowed"));
            continue;
        }
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let first = line.split([' ', '(', ',']).next().unwrap_or_default();
        if first.is_empty() || CONSTRAINTS.iter().any(|k| k.eq_ignore_ascii_case(first)) {
            continue;
        }
        columns.push(first.to_string());
    }
    tables
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_would_merge_verdict_from_before_is_reopened_on_upgrade() {
        // Verdicts recorded while acting was switched off. There is no such
        // switch now: every verdict is acted on, so these go back to pending
        // and are judged again — never left stranded in a state nothing reads.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let ids = store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "a".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "b".into(),
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
        store
            .record_pair(&ids[0].id, &ids[1].id, 0.9)
            .await
            .unwrap();
        sqlx::query("UPDATE artifact_pairs SET state = 'would_merge'")
            .execute(&store.pool)
            .await
            .unwrap();

        store.migrate().await.unwrap();

        let states: Vec<String> = sqlx::query_scalar("SELECT state FROM artifact_pairs")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(states, vec!["pending".to_string()]);
    }

    #[tokio::test]
    async fn a_deployed_base_gains_source_url_without_being_recreated() {
        // The capture-surfaces column. A knowledge base is expensive to
        // rebuild — every artifact in it was paid for in GPU time — so a
        // nullable column with no default must never be the reason an operator
        // is asked to recreate one.
        let store = Store::memory().await.unwrap();
        sqlx::query("ALTER TABLE corpora DROP COLUMN source_url")
            .execute(&store.pool)
            .await
            .expect("the fixture needs a corpora table without source_url");

        store
            .migrate()
            .await
            .expect("migrate refused a base captured before source_url existed");

        let has: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('corpora') WHERE name = 'source_url'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(has, 1, "source_url was never added to the existing table");

        // And a capture still works against the upgraded base.
        let c = store
            .insert_corpus_with_signature(
                "alpha\n\nbeta",
                "web",
                None,
                vec![],
                None,
                &serde_json::json!({}),
                crate::store::corpora::Followup::Nothing,
            )
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(store.get_corpus(&c.id).await.unwrap().source_url, None);
    }

    #[tokio::test]
    async fn a_column_added_after_deployment_reaches_a_database_that_predates_it() {
        // The upgrade path, and the trap in it. `CREATE TABLE IF NOT EXISTS` is
        // a no-op against a table that already exists, so a column added to
        // schema.sql never reaches a running base — and the check at the end of
        // migrate() then refuses to start it. An operator would have been asked
        // to recreate a knowledge base to gain a column with a default.
        let store = Store::memory().await.unwrap();
        // The index names the column, so it goes first — which is the same
        // ordering constraint migrate() itself has to respect.
        sqlx::query("DROP INDEX IF EXISTS idx_jobs_claim2")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE jobs DROP COLUMN seq")
            .execute(&store.pool)
            .await
            .expect("the fixture needs a jobs table without seq");

        store
            .migrate()
            .await
            .expect("migrate refused an older base");

        let has_seq: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = 'seq'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(has_seq, 1, "seq was never added to the existing table");

        // And the default has to be usable, or every pre-existing row would sort
        // as NULL and the claim ordering would be undefined for them.
        store
            .enqueue(jobs::Stage::Embed, "corpus", "c-1")
            .await
            .unwrap();
        let seq = store
            .job_seq(jobs::Stage::Embed, "c-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seq, 0);
    }

    #[tokio::test]
    async fn a_base_captured_before_merging_gains_the_provenance_columns() {
        // The three columns merging added to `artifacts`. All of them have
        // defaults that say exactly what was true of every artifact predating
        // merging — captured, from a corpus, nothing outstanding — so this is an
        // append, and 608 artifacts bought with GPU time must not be recreated
        // to gain it.
        let store = Store::memory().await.unwrap();
        // Both indexes name a column being dropped, so they go first — the same
        // ordering constraint migrate() respects by running the appends before
        // it applies schema.sql.
        for stmt in [
            "DROP INDEX IF EXISTS idx_artifacts_dirty",
            "DROP INDEX IF EXISTS idx_artifacts_provenance",
            "ALTER TABLE artifacts DROP COLUMN provenance",
            "ALTER TABLE artifacts DROP COLUMN source_count",
            "ALTER TABLE artifacts DROP COLUMN lifecycle_dirty",
        ] {
            sqlx::query(stmt)
                .execute(&store.pool)
                .await
                .expect("the fixture needs an artifacts table from before merging");
        }

        store
            .migrate()
            .await
            .expect("migrate refused a base captured before merging");

        for column in ["provenance", "source_count", "lifecycle_dirty"] {
            let has: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pragma_table_info('artifacts') WHERE name = ?",
            )
            .bind(column)
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(has, 1, "{column} was never added to the existing table");
        }

        // And the defaults have to be the truth about an artifact that predates
        // merging, or every one of them reads back as a merge with no sources.
        let c = store
            .insert_corpus_with_signature(
                "alpha\n\nbeta",
                "web",
                None,
                vec![],
                None,
                &serde_json::json!({}),
                crate::store::corpora::Followup::Nothing,
            )
            .await
            .unwrap()
            .into_corpus();
        let made = store
            .insert_artifacts(
                &c.id,
                &[artifacts::NewArtifact {
                    ordinal: 0,
                    text: "alpha".into(),
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
        let got = store.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(got.provenance, artifacts::Provenance::Captured);
        assert_eq!(got.source_count, 0);
    }

    #[tokio::test]
    async fn migrate_folds_categories_off_the_list_into_other() {
        // Rows written while the kind was a free string hold subject words, and
        // the filter row is built from whatever is stored — so until these are
        // folded, closing the schema changes nothing an operator can see.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[
                    artifacts::NewArtifact {
                        ordinal: 0,
                        text: "alpha".into(),
                        corpus_span: None,
                        title: None,
                        category: Some("Forensic Science / Criminalistics".into()),
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    artifacts::NewArtifact {
                        ordinal: 1,
                        text: "bravo".into(),
                        corpus_span: None,
                        title: None,
                        category: Some("procedure".into()),
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();

        store.migrate().await.unwrap();

        let mut kinds: Vec<String> =
            sqlx::query_scalar("SELECT category FROM artifacts ORDER BY ordinal")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        kinds.sort();
        assert_eq!(
            kinds,
            vec!["other".to_string(), "procedure".to_string()],
            "a subject word folds to `other`; a form word is left alone"
        );
    }

    #[tokio::test]
    async fn a_base_captured_before_activation_gains_it_with_a_real_stamp() {
        // The append can only state a constant, and zero is the wrong stamp:
        // it reads as an artifact last reached in 1970. The backfill is what
        // makes the column true of a base that predates it.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[artifacts::NewArtifact {
                    ordinal: 0,
                    text: "alpha".into(),
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
        for stmt in [
            "ALTER TABLE artifacts DROP COLUMN activation",
            "ALTER TABLE artifacts DROP COLUMN activated_at",
        ] {
            sqlx::query(stmt)
                .execute(&store.pool)
                .await
                .expect("the fixture needs an artifacts table from before activation");
        }

        store
            .migrate()
            .await
            .expect("migrate refused a base captured before activation");

        let (value, stamp): (f64, i64) =
            sqlx::query_as("SELECT activation, activated_at FROM artifacts")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!((value - 1.0).abs() < 1e-9);
        assert!(stamp > 0, "activated_at was left at the epoch");

        // ...and the same adoption decides where the sweep starts reading. A
        // base that has been recording searches for months must not have that
        // whole log folded into links on its first tick.
        for key in ["associate.events_after", "associate.judged_after"] {
            let mark: i64 = store
                .meta_get(key)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{key} was left unseeded on an adopting base"))
                .parse()
                .unwrap();
            assert!(mark > 0, "{key} was seeded at the epoch");
        }

        // Once seeded, a later boot leaves the watermark where the sweep put
        // it — re-seeding would skip everything learned since.
        store
            .meta_set("associate.events_after", "42")
            .await
            .unwrap();
        store.migrate().await.unwrap();
        assert_eq!(
            store.meta_get("associate.events_after").await.unwrap(),
            Some("42".into()),
            "a later boot moved the watermark"
        );
    }

    #[tokio::test]
    async fn a_base_created_with_activation_is_not_treated_as_adopting_it() {
        // Nothing to backfill and nothing to skip: a database created by this
        // version has no search log predating the feature, and its watermarks
        // start where every other counter does.
        let store = Store::memory().await.unwrap();
        store.migrate().await.unwrap();
        assert_eq!(
            store.meta_get("associate.events_after").await.unwrap(),
            None
        );
        assert_eq!(
            store.meta_get("associate.judged_after").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_base_whose_corpus_id_is_still_not_null_is_named_as_such() {
        // The change an append cannot make. A merged artifact belongs to no
        // corpus, so `corpus_id` had to become nullable, and SQLite cannot relax
        // that in place. The failure has to arrive at startup naming the table
        // to rebuild — not at the first merge, as a bare constraint violation.
        let store = Store::memory().await.unwrap();
        // Put the constraint back the way a pre-merging base declared it. Both
        // indexes name the column, so they come out first and the schema puts
        // them back.
        for stmt in [
            "DROP INDEX IF EXISTS idx_artifacts_corpus",
            "DROP INDEX IF EXISTS idx_artifacts_window",
            "ALTER TABLE artifacts DROP COLUMN corpus_id",
            "ALTER TABLE artifacts ADD COLUMN corpus_id TEXT NOT NULL DEFAULT ''",
        ] {
            sqlx::query(stmt)
                .execute(&store.pool)
                .await
                .expect("the fixture needs an artifacts table from before merging");
        }

        let err = store
            .migrate()
            .await
            .expect_err("migrate accepted a base that cannot hold a merged artifact");
        let msg = err.to_string();
        assert!(
            msg.contains("corpus_id") && msg.contains("rebuilt"),
            "the error has to name the column and the remedy, got: {msg}"
        );
    }

    #[tokio::test]
    async fn applying_the_schema_twice_changes_nothing() {
        // migrate() runs on every connect, so a second application has to be a
        // no-op rather than an error. This is what `IF NOT EXISTS` on every
        // object buys, and it is the whole reason the schema can be a
        // statement of shape rather than a chain of diffs.
        let store = Store::memory().await.unwrap();
        let before: Vec<(String, String)> =
            sqlx::query_as("SELECT type, name FROM sqlite_master ORDER BY type, name")
                .fetch_all(&store.pool)
                .await
                .unwrap();

        store.migrate().await.unwrap();
        store.migrate().await.unwrap();

        let after: Vec<(String, String)> =
            sqlx::query_as("SELECT type, name FROM sqlite_master ORDER BY type, name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        assert!(
            before.iter().any(|(_, n)| n == "artifacts"),
            "the schema must actually have been applied"
        );
    }

    #[test]
    fn the_schema_parses_into_the_columns_it_declares() {
        let tables = schema_columns(include_str!("schema.sql"));
        let segments = tables
            .iter()
            .find(|(t, _)| t == "segments")
            .expect("segments is in the schema");
        assert_eq!(
            segments.1,
            vec![
                "corpus_id",
                "idx",
                "start_line",
                "end_line",
                "text",
                "carry_lines",
                "state",
                "attempts",
                "last_error",
            ],
            "comments and the PRIMARY KEY line must not be read as columns"
        );
        assert!(
            tables.len() >= 9,
            "every CREATE TABLE must be found, got {}",
            tables.len()
        );
    }

    #[tokio::test]
    async fn a_database_missing_a_column_is_refused_with_the_column_named() {
        // The failure this replaces: `CREATE TABLE IF NOT EXISTS` leaves an
        // older table exactly as it is, so startup succeeded and every corpus
        // view, synthesis run and reconcile job then panicked on a column that
        // was never added.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE segments (
               corpus_id TEXT NOT NULL, idx INTEGER NOT NULL,
               start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
               state TEXT NOT NULL DEFAULT 'pending',
               attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
               PRIMARY KEY (corpus_id, idx))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = Store {
            pool,
            capture: Default::default(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(err.contains("segments.text"), "unhelpful message: {err}");
        assert!(err.contains("segments.carry_lines"), "{err}");
    }

    #[tokio::test]
    async fn a_fresh_file_database_gets_the_whole_schema() {
        let dir = std::env::temp_dir().join(format!("engram-schema-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engram.db");
        let cfg = crate::config::StoreConfig {
            path: path.to_str().unwrap().to_string(),
        };

        let store = Store::connect(&cfg).await.unwrap();
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();
        for expected in [
            "api_tokens",
            "artifact_pairs",
            "artifacts",
            "corpora",
            "jobs",
            "search_candidates",
            "search_events",
            "segments",
            "sessions",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} is missing: {names:?}"
            );
        }

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_associative_fixups_finish_on_a_later_boot_when_the_adopting_one_did_not() {
        // The boot that adds the columns can fail before the fixups below them:
        // the `corpus_id` check returns `Err` after both ALTERs have committed,
        // and a kill in the same window does the same. On the boot after, the
        // columns are already there — so anything keyed on "this call added
        // them" is false forever, the stamps stay at the epoch and the sweep's
        // watermarks are never written. An absent watermark reads as "from the
        // epoch", which folds the entire historical search log in on one tick.
        // Keyed on the state of the database, so a later boot finishes the job.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let ids = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
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
        // Exactly what an aborted adopting boot leaves behind: the columns
        // present, the stamps unbackfilled, the watermarks unwritten.
        sqlx::query("UPDATE artifacts SET activated_at = 0")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM meta WHERE key LIKE 'associate.%'")
            .execute(&store.pool)
            .await
            .unwrap();

        store.migrate().await.unwrap();

        let stamp: i64 = sqlx::query_scalar("SELECT activated_at FROM artifacts WHERE id = ?")
            .bind(&ids[0].id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_ne!(
            stamp, 0,
            "every artifact still reads as decayed to nothing since 1970"
        );
        for key in [
            crate::jobs::associate::EVENTS_AFTER,
            crate::jobs::associate::JUDGED_AFTER,
        ] {
            assert!(
                store.meta_get(key).await.unwrap().is_some(),
                "{key} was never seeded; the first sweep replays the whole log"
            );
        }
    }
}
