//! Every built-in decision question: id, version, wording, options.
//! Wording is behaviour: a change of instructions or options bumps the version.

use super::Asked;
use crate::infer::decide::{Kind, Question};
use crate::infer::prompt::CATEGORIES;

fn choice(id: &'static str, swapped: bool, instructions: &str, options: &[(&str, &str)]) -> Asked {
    let options = options
        .iter()
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect();
    Asked {
        id,
        version: 1,
        question: Question {
            key: id.into(),
            kind: Kind::Choice { options },
            instructions: instructions.into(),
        },
        swapped,
    }
}

fn noul(id: &'static str, instructions: &str) -> Asked {
    Asked {
        id,
        version: 1,
        question: Question {
            key: id.into(),
            kind: Kind::Noul,
            instructions: instructions.into(),
        },
        swapped: false,
    }
}

pub fn pair_relation() -> Asked {
    choice(
        "pair_relation",
        true,
        "How do A and B relate?",
        &[
            (
                "duplicate",
                "they state the same thing; nothing in one is missing in the other's subject",
            ),
            (
                "a_replaces_b",
                "A is newer or corrected and makes B outdated",
            ),
            (
                "b_replaces_a",
                "B is newer or corrected and makes A outdated",
            ),
            (
                "conflict",
                "they give different values or instructions for the same thing and neither is clearly newer",
            ),
            (
                "distinct",
                "different subjects, or the same subject with different content worth keeping apart",
            ),
            ("vacuous", "neither states anything a reader could use"),
        ],
    )
}

pub fn link_kind() -> Asked {
    choice(
        "link_kind",
        true,
        "These two notes keep being found by the same searches. How are they related?",
        &[
            ("continues", "one is the next step after the other"),
            ("example", "one is an instance of what the other describes"),
            ("alternative", "two ways to the same end"),
            ("background", "one explains what the other uses"),
            (
                "contradicts",
                "they say different things about the same subject",
            ),
            ("duplicate", "they say the same thing"),
            ("unrelated", "no connection worth showing"),
        ],
    )
}

pub fn link_direction() -> Asked {
    choice(
        "link_direction",
        true,
        "Which note comes first: the earlier step, the general description, or the explanation?",
        &[("a_first", "A"), ("b_first", "B")],
    )
}

pub fn reap_worth() -> Asked {
    noul(
        "reap_worth",
        "Does the retired note state anything a reader could use that the live notes shown do not?",
    )
}

pub fn category() -> Asked {
    let described = [
        "an idea and how to think about it",
        "steps to do something",
        "facts to look up",
        "a piece of code or a command",
        "settings and their values",
        "what a term means",
        "a worked case",
        "none of these",
    ];
    let options: Vec<(&str, &str)> = CATEGORIES.iter().copied().zip(described).collect();
    choice("category", false, "What kind of note is this?", &options)
}

pub fn pinned() -> Asked {
    noul(
        "pinned",
        "Does this record a decision or a commitment its writer made?",
    )
}

pub fn time_bound() -> Asked {
    noul(
        "time_bound",
        "Does this state something that will go out of date: a version, a price, a schedule, a temporary workaround?",
    )
}

pub fn self_contained() -> Asked {
    noul(
        "self_contained",
        "Can this be understood without the document it came from?",
    )
}

pub fn asks_reminder() -> Asked {
    noul(
        "asks_reminder",
        "Does this ask to be reminded of something?",
    )
}

pub fn has_date() -> Asked {
    noul(
        "has_date",
        "Does this mention a specific future date or deadline?",
    )
}

pub fn answerable() -> Asked {
    noul(
        "answerable",
        "Do these excerpts contain the answer to these questions?",
    )
}

pub fn complete() -> Asked {
    noul(
        "complete",
        "Does the rewrite state everything the originals state?",
    )
}

pub fn adds_nothing() -> Asked {
    noul(
        "adds_nothing",
        "Does the rewrite state only what the originals state?",
    )
}

pub fn answers_query(q: &str) -> Asked {
    noul(
        "answers_query",
        &format!("Would this note help someone who searched for: «{q}»?"),
    )
}

pub fn all() -> Vec<Asked> {
    vec![
        pair_relation(),
        link_kind(),
        link_direction(),
        reap_worth(),
        category(),
        pinned(),
        time_bound(),
        self_contained(),
        asks_reminder(),
        has_date(),
        answerable(),
        complete(),
        adds_nothing(),
        answers_query(""),
    ]
}

pub fn by_id(id: &str) -> Option<Asked> {
    all().into_iter().find(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::decide::{Kind, swapped_questions, validate};
    use crate::infer::prompt::CATEGORIES;

    fn names(a: &Asked) -> Vec<String> {
        match &a.question.kind {
            Kind::Choice { options } => options.iter().map(|(n, _)| n.clone()).collect(),
            _ => panic!("not a choice"),
        }
    }

    #[test]
    fn every_question_is_valid_and_keyed_by_its_id() {
        for a in all() {
            validate(std::slice::from_ref(&a.question)).unwrap();
            assert_eq!(a.question.key, a.id);
            assert_eq!(a.version, 1);
        }
    }

    #[test]
    fn ids_are_unique_and_fit_a_key() {
        let ids: std::collections::HashSet<_> = all().iter().map(|a| a.id).collect();
        assert_eq!(ids.len(), all().len());
        assert_eq!(all().len(), 14);
        for id in ids {
            assert!(id.len() <= 30 && id.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn pair_relation_offers_the_six_relations_in_the_specs_order() {
        let a = pair_relation();
        assert!(a.swapped);
        assert_eq!(
            names(&a),
            [
                "duplicate",
                "a_replaces_b",
                "b_replaces_a",
                "conflict",
                "distinct",
                "vacuous"
            ]
        );
        let Kind::Choice { options } = &a.question.kind else {
            panic!()
        };
        assert!(options.iter().all(|(_, d)| !d.is_empty()));
    }

    #[test]
    fn link_kind_offers_seven_kinds_and_direction_two() {
        assert_eq!(names(&link_kind()).len(), 7);
        assert_eq!(names(&link_direction()), ["a_first", "b_first"]);
    }

    #[test]
    fn category_describes_each_of_the_eight_and_is_not_swapped() {
        let a = category();
        assert!(!a.swapped);
        assert_eq!(names(&a), CATEGORIES);
        let Kind::Choice { options } = &a.question.kind else {
            panic!()
        };
        assert!(options.iter().all(|(_, d)| !d.is_empty()));
    }

    #[test]
    fn answers_query_quotes_the_search() {
        assert_eq!(
            answers_query("mount loop").question.instructions,
            "Would this note help someone who searched for: «mount loop»?"
        );
    }

    #[test]
    fn by_id_finds_each_and_refuses_the_unknown() {
        for a in all() {
            assert_eq!(by_id(a.id).unwrap().id, a.id);
        }
        assert!(by_id("nope").is_none());
    }

    #[test]
    fn a_swapped_question_with_its_twin_still_fits_one_request() {
        let qs: Vec<_> = [pair_relation(), link_kind(), link_direction()]
            .into_iter()
            .map(|a| a.question)
            .collect();
        assert!(swapped_questions(&qs).len() <= 16);
    }
}
