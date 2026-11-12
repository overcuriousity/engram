//! Every API list answers this object, never a bare array, so a route can start paging without changing shape.

use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Debug, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Passed back as `?after=`; `null` on the last page and on bounded lists. Opaque on purpose.
    pub next: Option<String>,
}

impl<T> Page<T> {
    /// A list that is all there is.
    pub fn whole(items: Vec<T>) -> Self {
        Page { items, next: None }
    }

    /// The extra row says there is another page without a second query; it is not served.
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

/// A keyset, not an offset: lists grow at the head, and an offset would repeat a row.
#[derive(Debug, PartialEq)]
pub struct Cursor {
    pub at: i64,
    pub id: String,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.at, self.id))
    }

    /// A `400` for anything this server did not issue; the value comes from a query string.
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
