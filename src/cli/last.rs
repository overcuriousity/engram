//! What the last search printed, so a rank in it can be named later.
//!
//! `engram -s` is one process and `engram --show 3` is another, and nothing
//! between them remembers which artifact was third. A small file does: the ids
//! in the order they were drawn, the question they answered, and when. It holds
//! no text and no scores — everything in it is already on the operator's screen.

use crate::error::{Error, Result};
use std::path::PathBuf;

/// The ranked ids of one search, in the order they were printed.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct LastSearch {
    pub query: String,
    pub at: i64,
    pub ids: Vec<String>,
}

/// Where the list is kept: `$XDG_STATE_HOME/engram/last-search.json`, or the
/// path the specification says that variable defaults to.
///
/// State rather than config or cache: it is neither something the operator
/// edits nor something that can be regenerated, which is exactly what the XDG
/// state directory is for.
///
/// `get` is passed rather than read for the reason `Face::decide` takes its
/// facts as arguments — two tests must not race on one process's environment.
pub fn path_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = match get("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(get("HOME").filter(|v| !v.is_empty())?)
            .join(".local")
            .join("state"),
    };
    Some(base.join("engram").join("last-search.json"))
}

/// `path_from`, against this process.
pub fn path() -> Option<PathBuf> {
    path_from(|k| std::env::var(k).ok())
}

/// Whether a search is one anybody will name a rank out of.
///
/// A search in a pipeline is not, and neither is `--json`, which is read by a
/// machine even when a person is watching it arrive. Both would otherwise
/// overwrite the list somebody is working from in another window.
pub fn worth_remembering(is_tty: bool, json: bool) -> bool {
    is_tty && !json
}

/// Remember what was drawn. Failure is silent by design: a search that found
/// its hits succeeded, whatever the state directory did, and an error here
/// would report a working search as broken.
pub fn save_to(p: &std::path::Path, query: &str, ids: Vec<String>) {
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let held = LastSearch {
        query: query.to_string(),
        at,
        ids,
    };
    if let Ok(text) = serde_json::to_string(&held) {
        std::fs::write(p, text).ok();
    }
}

/// `save_to`, at the path this process keeps it.
pub fn save(query: &str, ids: Vec<String>) {
    if let Some(p) = path() {
        save_to(&p, query, ids);
    }
}

/// What was drawn last, or `None` when nothing was or the file cannot be read.
pub fn load_from(p: &std::path::Path) -> Option<LastSearch> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// `load_from`, at the path this process keeps it.
pub fn load() -> Option<LastSearch> {
    load_from(&path()?)
}

/// Turn what the operator typed into one artifact id.
///
/// Three forms, because three are what a person actually types: a rank from the
/// list still on screen, a leading piece of an id, or the whole id pasted from
/// somewhere else. The first two need the remembered list; the last one does
/// not, and must keep working in a shell that has never run a search.
pub fn resolve(needle: &str, last: Option<&LastSearch>) -> Result<String> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Err(Error::Validation("--show needs a rank or an id".into()));
    }

    // A rank first: a small integer is never an id, and reading it as one would
    // send `3` to the server as a lookup that can only 404.
    if let Ok(n) = needle.parse::<usize>() {
        let Some(last) = last else {
            return Err(Error::Validation(format!(
                "`--show {n}` names a hit from the last search, and no search \
                 has been run from this shell yet"
            )));
        };
        return match n.checked_sub(1).and_then(|i| last.ids.get(i)) {
            Some(id) => Ok(id.clone()),
            None => Err(Error::Validation(format!(
                "the last search — {} — had {} hit(s), so there is no {n}",
                last.query,
                last.ids.len()
            ))),
        };
    }

    // A prefix, against what was drawn. Checked before the pass-through so that
    // a prefix naming two hits is refused rather than sent to the server, where
    // it would 404 and say nothing about the ambiguity.
    if let Some(last) = last {
        let matched: Vec<&String> = last
            .ids
            .iter()
            .filter(|id| id.starts_with(needle))
            .collect();
        match matched.as_slice() {
            [one] => return Ok((*one).clone()),
            [] => {}
            many => {
                return Err(Error::Validation(format!(
                    "`{needle}` names {} hits of the last search; type more of it",
                    many.len()
                )));
            }
        }
    }

    // Neither a rank nor a prefix of anything remembered: it is an id, and the
    // server is the one that decides whether it exists.
    Ok(needle.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        }
    }

    fn last(ids: &[&str]) -> LastSearch {
        LastSearch {
            query: "nuix forensik".into(),
            at: 1_787_000_000,
            ids: ids.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// The rule that keeps a script from overwriting the list a person is
    /// working from. A search in a pipeline is not a search anyone will name a
    /// rank out of, and `--json` is read by a machine even on a terminal.
    #[test]
    fn only_a_search_a_person_watched_is_remembered() {
        assert!(worth_remembering(true, false));
        assert!(!worth_remembering(false, false), "a pipe");
        assert!(
            !worth_remembering(true, true),
            "--json is read by a machine"
        );
    }

    #[test]
    fn what_a_search_remembered_is_what_show_reads_back() {
        let dir = std::env::temp_dir().join(format!("engram-last-{}", std::process::id()));
        let p = dir.join("deep").join("last-search.json");
        std::fs::remove_dir_all(&dir).ok();

        save_to(&p, "nuix forensik", vec!["id-a".into(), "id-b".into()]);
        let back = load_from(&p).expect("the list that was just written");

        assert_eq!(back.query, "nuix forensik");
        assert_eq!(back.ids, vec!["id-a".to_string(), "id-b".to_string()]);
        assert_eq!(
            resolve("2", Some(&back)).unwrap(),
            "id-b",
            "the whole point of writing it"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_remembered_is_not_a_failure() {
        let p = std::env::temp_dir().join("engram-no-such-list-at-all.json");
        std::fs::remove_file(&p).ok();
        assert!(load_from(&p).is_none());
    }

    #[test]
    fn the_rank_the_list_printed_is_the_rank_show_reads() {
        let held = last(&["id-a", "id-b", "id-c"]);
        assert_eq!(resolve("3", Some(&held)).unwrap(), "id-c");
        assert_eq!(
            resolve("1", Some(&held)).unwrap(),
            "id-a",
            "the list is one-based on screen and must be one-based here"
        );
    }

    #[test]
    fn a_rank_past_the_end_says_what_the_last_search_actually_had() {
        let held = last(&["id-a", "id-b"]);
        let said = resolve("7", Some(&held)).unwrap_err().to_string();
        assert!(said.contains("nuix forensik"), "{said}");
        assert!(said.contains('2'), "{said}");
    }

    /// Zero is off the end in the other direction, and `ids[0 - 1]` is the
    /// panic that would be.
    #[test]
    fn rank_zero_is_off_the_list_rather_than_a_panic() {
        assert!(resolve("0", Some(&last(&["id-a"]))).is_err());
    }

    #[test]
    fn a_rank_with_nothing_remembered_says_so_rather_than_asking_the_server() {
        let said = resolve("3", None).unwrap_err().to_string();
        assert!(said.contains("no search"), "{said}");
    }

    #[test]
    fn a_prefix_is_enough_when_it_names_one_hit_of_the_last_search() {
        let held = last(&["01a04209-3b06", "01a03a96-5a8a"]);
        assert_eq!(resolve("01a042", Some(&held)).unwrap(), "01a04209-3b06");
    }

    #[test]
    fn a_prefix_naming_two_hits_is_refused_rather_than_guessed() {
        let held = last(&["01a03a96-5a8a", "01a03a96-4a65"]);
        let said = resolve("01a03a96", Some(&held)).unwrap_err().to_string();
        assert!(said.contains("2 hits"), "{said}");
    }

    /// The id pasted from a citation list, in a shell that has never searched.
    #[test]
    fn a_whole_id_needs_no_remembered_search() {
        let id = "01a04209-3b06-7af1-aead-4fbf5dd0a4b4";
        assert_eq!(resolve(id, None).unwrap(), id);
        assert_eq!(
            resolve(id, Some(&last(&["something-else"]))).unwrap(),
            id,
            "a remembered list that does not hold it must not swallow it"
        );
    }

    #[test]
    fn the_state_file_follows_xdg_and_falls_back_to_where_xdg_says_it_would() {
        assert_eq!(
            path_from(env(&[("XDG_STATE_HOME", "/s"), ("HOME", "/home/u")])),
            Some(PathBuf::from("/s/engram/last-search.json"))
        );
        assert_eq!(
            path_from(env(&[("HOME", "/home/u")])),
            Some(PathBuf::from(
                "/home/u/.local/state/engram/last-search.json"
            )),
            "the default the XDG specification gives for that variable"
        );
        assert_eq!(
            path_from(env(&[("XDG_STATE_HOME", ""), ("HOME", "/home/u")])),
            Some(PathBuf::from(
                "/home/u/.local/state/engram/last-search.json"
            )),
            "an empty value is unset"
        );
        assert_eq!(path_from(env(&[])), None, "nowhere to keep it");
    }

    #[test]
    fn what_was_written_is_what_comes_back() {
        let held = last(&["id-a", "id-b"]);
        let text = serde_json::to_string(&held).unwrap();
        assert_eq!(serde_json::from_str::<LastSearch>(&text).unwrap(), held);
    }
}
