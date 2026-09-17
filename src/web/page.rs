//! The one shape every list in the API answers with, and the cursor that
//! pages it.
//!
//! Always an object and never a bare array, so a route that does not page
//! today can start without changing shape, and a client writes one pager.

use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Debug, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Passed back as `?after=` for the next page; `null` on the last one and
    /// on every list that is bounded rather than paged. Opaque on purpose: a
    /// client that never looks inside is a client the server can change the
    /// inside for.
    pub next: Option<String>,
}

impl<T> Page<T> {
    /// A list that is all there is.
    pub fn whole(items: Vec<T>) -> Self {
        Page { items, next: None }
    }

    /// One page out of `limit + 1` fetched rows: the extra row is how the page
    /// learns there is another without a second query, and it is not served.
    pub fn of(mut rows: Vec<T>, limit: usize, cursor: impl Fn(&T) -> Cursor) -> Self {
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = match (more, rows.last()) {
            (true, Some(last)) => Some(cursor(last).encode()),
            _ => None,
        };
        Page { items: rows, next }
    }
}

/// A place in a list ordered by `(at, id)`.
///
/// A keyset and not an offset: these lists grow at the head, and a phone that
/// captures and then opens the list is the ordinary case. An offset counted
/// from the top would serve the last row of page one again as the first row
/// of page two.
#[derive(Debug, PartialEq)]
pub struct Cursor {
    pub at: i64,
    pub id: String,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.at, self.id))
    }

    /// A `400` for anything this server did not issue. Never a `500`: the
    /// value arrives in a query string, and a query string can say anything.
    pub fn decode(s: &str) -> Result<Cursor> {
        let bad = || Error::Validation("after: not a cursor this server issued".into());
        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| bad())?;
        let text = String::from_utf8(bytes).map_err(|_| bad())?;
        let (at, id) = text.split_once(':').ok_or_else(bad)?;
        let at = at.parse().map_err(|_| bad())?;
        if id.is_empty() {
            return Err(bad());
        }
        Ok(Cursor {
            at,
            id: id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_survives_the_round_trip() {
        let c = Cursor {
            at: 1_757_308_800,
            id: "01J:with:colons".into(),
        };
        assert_eq!(Cursor::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn anything_else_is_a_validation_error() {
        for s in ["", "%%%", "bm90LWEtY3Vyc29y", "OmlkLW9ubHk", "MTIzOg"] {
            assert!(
                matches!(Cursor::decode(s), Err(Error::Validation(_))),
                "`{s}` was taken for a cursor"
            );
        }
    }

    #[test]
    fn a_page_serves_limit_rows_and_points_at_the_last_of_them() {
        let page = Page::of(vec![3, 2, 1], 2, |n| Cursor {
            at: *n,
            id: "x".into(),
        });
        assert_eq!(page.items, vec![3, 2]);
        assert_eq!(Cursor::decode(&page.next.unwrap()).unwrap().at, 2);
        assert!(
            Page::of(vec![2, 1], 2, |n| Cursor {
                at: *n,
                id: "x".into()
            })
            .next
            .is_none()
        );
    }
}
