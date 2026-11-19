
use crate::types::{Kind, Query};

/// `alternateQuery` derives the fallback query. `None` mirrors Go's
/// `(Query{}, false)`.
pub fn alternate_query(q: &Query) -> Option<Query> {
    if q.kind != Kind::SERIES {
        return None;
    }
    if !q.title.contains(" - ") {
        return None;
    }
    let normalized = q.title.replace(" - ", ": ").trim().to_string();
    if normalized.is_empty() {
        return None;
    }
    let mut alt = q.clone();
    alt.title = normalized;
    Some(alt)
}
