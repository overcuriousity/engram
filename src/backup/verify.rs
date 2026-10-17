//! Whether the stores agree: `engram --verify-stores [--user S]`, and the
//! check that ends every `--restore`. One table and one word (spec §5).
//!
//! Alone it writes nothing. At the end of a restore it also reconciles: points
//! no row names are pruned and embedded rows without a point are re-armed —
//! the two halves of `heal_store_drift`, run now so the report can say what it
//! did, and computed by the same `store_drift`.

use super::generations::{AliasState, Generations};
use crate::config::Config;
use crate::core::ingest::store_drift;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::store::control::{Control, User};
use crate::store::jobs::Stage;
use std::path::Path;

/// One cell of the table.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Pass(String),
    /// Restored, with `n` embeds queued to close the gap.
    Repair(u64, String),
    Fail(String),
}

#[derive(Debug)]
pub struct TenantRow {
    pub slug: String,
    pub cells: Vec<(&'static str, Cell)>,
}

#[derive(Debug)]
pub struct VerifyReport {
    pub tenants: Vec<TenantRow>,
    /// `.db` files in `store.dir` the control database does not name. Listed
    /// and left alone, and they do not decide the verdict: a full restore
    /// over a base with a user created after the backup would otherwise fail
    /// for ever on a file that is somebody's data.
    pub stray: Vec<String>,
    /// Whether this check wrote (pruned and re-armed) or only looked.
    pub reconciled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Everything is in place; this many embeds are queued.
    Repair(u64),
    Fail,
}

impl Verdict {
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::Repair(_) => 2,
            Verdict::Fail => 1,
        }
    }
}

impl VerifyReport {
    pub fn verdict(&self) -> Verdict {
        let cells = self.tenants.iter().flat_map(|t| t.cells.iter());
        let mut queued = 0;
        for (_, c) in cells {
            match c {
                Cell::Fail(_) => return Verdict::Fail,
                Cell::Repair(n, _) => queued += n,
                Cell::Pass(_) => {}
            }
        }
        if queued > 0 {
            Verdict::Repair(queued)
        } else {
            Verdict::Pass
        }
    }
}

impl std::fmt::Display for VerifyReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for t in &self.tenants {
            for (check, cell) in &t.cells {
                let (word, text) = match cell {
                    Cell::Pass(s) => ("PASS", s),
                    Cell::Repair(_, s) => ("REPAIR", s),
                    Cell::Fail(s) => ("FAIL", s),
                };
                writeln!(f, "{:<16} {:<16} {:<6} {text}", t.slug, check, word)?;
            }
        }
        if !self.stray.is_empty() {
            writeln!(
                f,
                "not named by the control database, left alone: {}",
                self.stray.join(", ")
            )?;
        }
        match self.verdict() {
            Verdict::Pass => write!(f, "PASS"),
            Verdict::Repair(n) => write!(
                f,
                "REPAIR: {n} embeds {} ({n} embedding calls)",
                if self.reconciled {
                    "queued"
                } else {
                    "owed; the next sweep queues them"
                }
            ),
            Verdict::Fail => write!(f, "FAIL"),
        }
    }
}

/// Check every user the control database names, or the one `user` names (a
/// slug or a subject). With `reconcile`, prune and re-arm as well.
///
/// `reembed`: a restore that skipped the snapshots on purpose. The recipe the
/// tenant database recorded is then expected to differ, and is not a failure.
pub async fn verify(
    cfg: &Config,
    user: Option<&str>,
    reconcile: bool,
    reembed: bool,
    gens: &dyn Generations,
) -> Result<VerifyReport> {
    let control_path = Path::new(&cfg.store.control_path);
    if !control_path.is_file() {
        return Err(Error::Validation(format!(
            "no control database at {}",
            control_path.display()
        )));
    }
    let control = Control::connect(&cfg.store.control_path).await?;
    let users = control.users().await?;
    let stray = stray_files(cfg, &users)?;
    let chosen: Vec<&User> = match user {
        None => users.iter().collect(),
        Some(u) => match users.iter().find(|x| x.slug == u || x.subject == u) {
            Some(x) => vec![x],
            None => {
                return Err(Error::Validation(format!(
                    "--user {u} names no user; the control database has: {}",
                    users
                        .iter()
                        .map(|x| x.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        },
    };
    let mut tenants = Vec::with_capacity(chosen.len());
    for u in chosen {
        tenants.push(TenantRow {
            slug: u.slug.clone(),
            cells: tenant(cfg, &control, u, reconcile, reembed, gens).await,
        });
    }
    control.pool.close().await;
    Ok(VerifyReport {
        tenants,
        stray,
        reconciled: reconcile,
    })
}

/// `*.db` in `store.dir` whose stem is no user's slug, the control database
/// itself aside when it lives there.
fn stray_files(cfg: &Config, users: &[User]) -> Result<Vec<String>> {
    let dir = Path::new(&cfg.store.dir);
    let control = Path::new(&cfg.store.control_path).canonicalize().ok();
    let mut stray = vec![];
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stray),
        Err(e) => return Err(Error::Store(format!("{}: {e}", dir.display()))),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::Store(format!("{}: {e}", dir.display())))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") || !path.is_file() {
            continue;
        }
        if control.is_some() && path.canonicalize().ok() == control {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !users.iter().any(|u| u.slug == stem) {
            stray.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    stray.sort();
    Ok(stray)
}

/// The spec §5 rows for one tenant. A row that cannot be checked because an
/// earlier one failed is left out rather than guessed at; the failure says why.
async fn tenant(
    cfg: &Config,
    control: &Control,
    user: &User,
    reconcile: bool,
    reembed: bool,
    gens: &dyn Generations,
) -> Vec<(&'static str, Cell)> {
    let mut cells = vec![];
    let path = Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
    if !path.is_file() {
        cells.push((
            "database",
            Cell::Fail(format!("{} is missing", path.display())),
        ));
        return cells;
    }
    cells.push(("database", Cell::Pass(format!("{}.db", user.slug))));

    let alias = format!("{}_{}", cfg.vector.collection, user.slug);
    let dim = cfg.infer.embed.dim;
    let live = match gens.state(&alias).await {
        Ok(AliasState::Live { generation, dim: d }) if d == dim => {
            cells.push(("alias", Cell::Pass(format!("{alias} -> {generation}"))));
            true
        }
        Ok(AliasState::Live { generation, dim: d }) => {
            cells.push((
                "alias",
                Cell::Fail(format!("{generation} is {d} wide, the config says {dim}")),
            ));
            false
        }
        Ok(AliasState::Dangling(g)) => {
            cells.push((
                "alias",
                Cell::Fail(format!("{alias} points at {g}, which does not exist")),
            ));
            false
        }
        Ok(AliasState::Absent) => {
            cells.push(("alias", Cell::Fail(format!("no alias {alias}"))));
            false
        }
        Err(e) => {
            cells.push(("alias", Cell::Fail(e.to_string())));
            false
        }
    };

    let store = match Store::connect(&path.to_string_lossy(), control.clone(), &user.subject).await
    {
        Ok(s) => s,
        Err(e) => {
            cells.push(("recipe", Cell::Fail(format!("cannot open: {e}"))));
            return cells;
        }
    };
    let want = cfg.infer.embed.fingerprint();
    cells.push((
        "recipe",
        match store.meta_get("embed.recipe").await {
            Ok(Some(r)) if r == want => Cell::Pass("matches the config".into()),
            Ok(None) => Cell::Pass("not recorded yet; the first open records it".into()),
            Ok(Some(_)) if reembed => Cell::Pass("differs; re-embedding under the config".into()),
            Ok(Some(_)) => Cell::Fail("differs from the configured embedding recipe".into()),
            Err(e) => Cell::Fail(e.to_string()),
        },
    ));
    if live {
        drift(&store, &alias, reconcile, gens, &mut cells).await;
    }
    store.pool.close().await;
    cells
}

/// The last three rows: rows without points, points without rows, parity.
async fn drift(
    store: &Store,
    alias: &str,
    reconcile: bool,
    gens: &dyn Generations,
    cells: &mut Vec<(&'static str, Cell)>,
) {
    let read = async {
        let vectors = gens.open(alias).await?;
        // Points first, rows second, as `heal_store_drift` reads them.
        let points = vectors.all_artifact_ids().await?;
        let rows = store.list_all_artifact_ids().await?;
        let embedded = store.list_embedded_artifact_ids().await?;
        Ok::<_, Error>((vectors, points, rows, embedded))
    };
    let (vectors, points, rows, embedded) = match read.await {
        Ok(r) => r,
        Err(e) => {
            cells.push(("drift", Cell::Fail(e.to_string())));
            return;
        }
    };
    let d = store_drift(&rows, &embedded, &points);

    let missing = d.missing_points.len() as u64;
    cells.push((
        "rows w/o point",
        if missing == 0 {
            Cell::Pass("0".into())
        } else if !reconcile {
            Cell::Repair(missing, format!("{missing}, the next sweep re-arms them"))
        } else {
            let mut armed = Ok(());
            for id in &d.missing_points {
                armed = store.rearm_idle_seq(Stage::Embed, "artifact", id, 0).await;
                if armed.is_err() {
                    break;
                }
            }
            match armed {
                Ok(()) => Cell::Repair(missing, format!("{missing}, re-armed")),
                Err(e) => Cell::Fail(format!("{missing}; re-arming failed: {e}")),
            }
        },
    ));

    let orphans = d.orphan_points.len();
    let mut pruned = false;
    cells.push((
        "points w/o row",
        if orphans == 0 {
            Cell::Pass("0".into())
        } else if !reconcile {
            Cell::Fail(format!(
                "{orphans}; --verify-stores writes nothing, a --restore prunes them"
            ))
        } else {
            match vectors.delete_artifacts(&d.orphan_points).await {
                Ok(()) => {
                    pruned = true;
                    Cell::Pass(format!("0 after pruning {orphans}"))
                }
                Err(e) => Cell::Fail(format!("{orphans}; prune failed: {e}")),
            }
        },
    ));

    let have = points.len() - if pruned { orphans } else { 0 };
    let want = embedded.len();
    cells.push((
        "points = embedded",
        if have == want {
            Cell::Pass(format!("{have}"))
        } else if have as u64 + missing == want as u64 {
            Cell::Repair(
                0,
                format!("{have} of {want}, off by the {missing} re-armed"),
            )
        } else {
            Cell::Fail(format!("{have} points, {want} embedded rows"))
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::generations::tests::MemGens;
    use crate::backup::restore::tests::{Base, backed_up, point};
    use std::sync::Arc;

    async fn check(base: &Base, reconcile: bool) -> VerifyReport {
        verify(&base.cfg, None, reconcile, false, &base.gens)
            .await
            .unwrap()
    }

    fn cell<'a>(r: &'a VerifyReport, slug: &str, check: &str) -> Option<&'a Cell> {
        r.tenants
            .iter()
            .find(|t| t.slug == slug)?
            .cells
            .iter()
            .find(|(c, _)| *c == check)
            .map(|(_, c)| c)
    }

    fn is_fail(c: Option<&Cell>) -> bool {
        matches!(c, Some(Cell::Fail(_)))
    }

    async fn served(gens: &MemGens, alias: &str) -> Arc<dyn crate::vector::VectorStore> {
        Generations::open(gens, alias).await.unwrap()
    }

    async fn jobs(base: &Base) -> i64 {
        let c = Control::connect(&base.cfg.store.control_path)
            .await
            .unwrap();
        let n = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        c.pool.close().await;
        n
    }

    #[tokio::test]
    async fn stores_that_agree_pass() {
        let base = backed_up().await;
        let r = check(&base, false).await;
        assert_eq!(r.verdict(), Verdict::Pass, "{r}");
        assert_eq!(r.verdict().exit_code(), 0);
        assert_eq!(r.tenants.len(), 2);
        assert!(r.to_string().ends_with("PASS"), "{r}");
    }

    #[tokio::test]
    async fn a_user_without_a_database_fails() {
        let base = backed_up().await;
        std::fs::remove_file(base.db(1)).unwrap();
        let r = check(&base, false).await;
        assert!(is_fail(cell(&r, &base.slugs[1], "database")), "{r}");
        assert_eq!(r.verdict().exit_code(), 1);
    }

    #[tokio::test]
    async fn a_database_no_user_names_is_listed_and_does_not_decide_the_verdict() {
        // A full restore over a base with a user created after the backup
        // leaves that user's file behind; failing on it would fail for ever.
        let base = backed_up().await;
        std::fs::copy(base.db(0), base.store_dir.join("zz-later.db")).unwrap();
        let r = check(&base, false).await;
        assert_eq!(r.stray, vec!["zz-later.db".to_string()]);
        assert_eq!(r.verdict(), Verdict::Pass, "{r}");
        assert!(r.to_string().contains("left alone: zz-later.db"), "{r}");
    }

    #[tokio::test]
    async fn an_absent_dangling_or_wrong_width_alias_fails() {
        let base = backed_up().await;
        base.gens.wipe(&base.alias(0));
        base.gens
            .gens
            .lock()
            .unwrap()
            .remove(&format!("{}_v1", base.alias(1)));
        let r = check(&base, false).await;
        for (i, want) in [(0, "no alias"), (1, "does not exist")] {
            match cell(&r, &base.slugs[i], "alias") {
                Some(Cell::Fail(s)) => assert!(s.contains(want), "{s}"),
                c => panic!("{c:?}"),
            }
        }
        assert_eq!(r.verdict(), Verdict::Fail);

        let mut base = backed_up().await;
        base.cfg.infer.embed.dim = 3;
        let r = check(&base, false).await;
        match cell(&r, &base.slugs[0], "alias") {
            Some(Cell::Fail(s)) => assert!(s.contains("2 wide, the config says 3"), "{s}"),
            c => panic!("{c:?}"),
        }
    }

    #[tokio::test]
    async fn a_recorded_recipe_other_than_the_configured_one_fails_unless_reembedding() {
        let mut base = backed_up().await;
        base.cfg.infer.embed.model = "another-model".into();
        let r = check(&base, false).await;
        assert!(is_fail(cell(&r, &base.slugs[0], "recipe")), "{r}");
        let r = verify(&base.cfg, None, false, true, &base.gens)
            .await
            .unwrap();
        assert!(
            matches!(cell(&r, &base.slugs[0], "recipe"), Some(Cell::Pass(_))),
            "{r}"
        );
    }

    #[tokio::test]
    async fn an_embedded_row_without_a_point_is_repair_and_only_a_reconcile_rearms_it() {
        let base = backed_up().await;
        served(&base.gens, &base.alias(0))
            .await
            .delete_artifacts(std::slice::from_ref(&base.embedded[0]))
            .await
            .unwrap();
        let r = check(&base, false).await;
        assert_eq!(r.verdict(), Verdict::Repair(1), "{r}");
        assert_eq!(r.verdict().exit_code(), 2);
        assert!(
            matches!(
                cell(&r, &base.slugs[0], "points = embedded"),
                Some(Cell::Repair(0, _))
            ),
            "off by exactly the re-armed one: {r}"
        );
        assert_eq!(jobs(&base).await, 0, "--verify-stores writes nothing");

        let r = check(&base, true).await;
        assert_eq!(r.verdict(), Verdict::Repair(1), "{r}");
        assert_eq!(jobs(&base).await, 1, "re-armed");
    }

    #[tokio::test]
    async fn a_point_without_a_row_fails_until_a_reconcile_prunes_it() {
        let base = backed_up().await;
        let v = served(&base.gens, &base.alias(1)).await;
        v.upsert(vec![point("no-such-row")]).await.unwrap();
        let r = check(&base, false).await;
        assert!(is_fail(cell(&r, &base.slugs[1], "points w/o row")), "{r}");
        assert_eq!(v.count().await.unwrap(), 2, "nothing pruned");

        let r = check(&base, true).await;
        assert_eq!(r.verdict(), Verdict::Pass, "{r}");
        assert_eq!(
            v.all_artifact_ids().await.unwrap(),
            vec![base.embedded[1].clone()]
        );
    }

    #[tokio::test]
    async fn points_that_do_not_match_the_embedded_rows_fail() {
        // A point for a row SQLite does not call embedded: neither drift set
        // names it, and only the count shows it.
        let base = backed_up().await;
        let other: String = {
            let c = sqlx::sqlite::SqlitePoolOptions::new()
                .connect(&format!("sqlite://{}", base.db(0).display()))
                .await
                .unwrap();
            let id = sqlx::query_scalar(
                "SELECT id FROM artifacts WHERE embed_state <> 'embedded' LIMIT 1",
            )
            .fetch_one(&c)
            .await
            .unwrap();
            c.close().await;
            id
        };
        served(&base.gens, &base.alias(0))
            .await
            .upsert(vec![point(&other)])
            .await
            .unwrap();
        let r = check(&base, false).await;
        assert!(
            is_fail(cell(&r, &base.slugs[0], "points = embedded")),
            "{r}"
        );
        assert_eq!(r.verdict().exit_code(), 1);
    }

    #[tokio::test]
    async fn user_narrows_the_check_to_one_tenant() {
        let base = backed_up().await;
        for who in [&base.slugs[1], &base.subjects[1]] {
            let r = verify(&base.cfg, Some(who), false, false, &base.gens)
                .await
                .unwrap();
            assert_eq!(r.tenants.len(), 1);
            assert_eq!(r.tenants[0].slug, base.slugs[1]);
        }
        let e = verify(&base.cfg, Some("nobody"), false, false, &base.gens)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("names no user"), "{e}");
    }
}
