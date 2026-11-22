//! Reference management for references.yaml.
//!
//! One entry per URL however it is written, and entries no file cites any more marked stale.

use crate::schema::{Reference, Status};
use std::collections::{HashMap, HashSet};
use url::Url;

/// The key that spellings of one URL share.
///
/// Two URLs are the same when they differ only in the case of the scheme or host, a default port, a trailing `/` on a path other than the root, a `#fragment` or `utm_*` parameters.
/// `http://` and `https://` stay different.
/// A URL that does not parse is its own key.
pub(crate) fn url_key(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_string();
    };
    parsed.set_fragment(None);
    let query = parsed.query().map(without_tracking);
    parsed.set_query(query.as_deref().filter(|q| !q.is_empty()));
    let path = parsed.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        parsed.set_path(path.trim_end_matches('/'));
    }
    parsed.to_string()
}

/// A query without its `utm_*` parameters, the others kept in order and as written
fn without_tracking(query: &str) -> String {
    let kept: Vec<&str> = query
        .split('&')
        .filter(|part| !part.is_empty() && !is_tracking(part))
        .collect();
    kept.join("&")
}

fn is_tracking(part: &str) -> bool {
    let name = part.split('=').next().unwrap_or_default();
    name.to_ascii_lowercase().starts_with("utm_")
}

/// How much a status says is wrong, so the worse one wins when two entries merge.
/// Not yet verified ranks above `ok`, so a merged entry is checked again rather than trusted.
fn severity(status: Status) -> u8 {
    match status {
        Status::Stale => 0,
        Status::Ok => 1,
        Status::Pending => 2,
        Status::Paywall => 3,
        Status::Login => 4,
        Status::Redirect => 5,
        Status::Dead => 6,
    }
}

/// Add the items of `more` that `list` lacks, in order
fn add_missing(list: &mut Vec<String>, more: Vec<String>) {
    for item in more {
        if !list.contains(&item) {
            list.push(item);
        }
    }
}

/// Fold `other`, the same URL written another way, into `into`.
/// Citations and categories are combined, a real title beats a bare URL, and the worse status is kept with its date and notes.
fn absorb(into: &mut Reference, other: Reference) {
    add_missing(&mut into.cited_in, other.cited_in);
    add_missing(&mut into.categories, other.categories);
    if into.title == into.url && other.title != other.url {
        into.title = other.title;
    }
    if severity(other.status) > severity(into.status) {
        into.status = other.status;
        into.verified = other.verified;
        into.notes = other.notes;
    }
}

/// References with at most one entry per URL key, in the order first seen
pub(crate) struct RefSet {
    refs: Vec<Reference>,
    by_key: HashMap<String, usize>,
    /// Spellings folded into an entry written another way
    folded: HashSet<String>,
}

impl RefSet {
    /// The entries of a references file, duplicates merged into the first of them
    pub(crate) fn new(refs: Vec<Reference>) -> Self {
        let mut set = Self {
            refs: Vec::with_capacity(refs.len()),
            by_key: HashMap::new(),
            folded: HashSet::new(),
        };
        for reference in refs {
            match set.find(&reference.url) {
                Some(idx) => {
                    set.folded.insert(reference.url.clone());
                    absorb(&mut set.refs[idx], reference);
                }
                None => {
                    set.push(reference);
                }
            }
        }
        set
    }

    /// The number of entries
    pub(crate) fn len(&self) -> usize {
        self.refs.len()
    }

    /// The index of the entry for `url` however it is written, noting the spelling if it differs
    pub(crate) fn find(&mut self, url: &str) -> Option<usize> {
        let idx = *self.by_key.get(&url_key(url))?;
        if self.refs[idx].url != url {
            self.folded.insert(url.to_string());
        }
        Some(idx)
    }

    /// Add an entry whose URL no entry has, and return its index
    pub(crate) fn push(&mut self, reference: Reference) -> usize {
        let idx = self.refs.len();
        self.by_key.insert(url_key(&reference.url), idx);
        self.refs.push(reference);
        idx
    }

    /// The entry at `idx`
    pub(crate) fn get_mut(&mut self, idx: usize) -> &mut Reference {
        &mut self.refs[idx]
    }

    /// The number of spellings folded into an entry written another way
    pub(crate) fn merged(&self) -> usize {
        self.folded.len()
    }

    /// Drop the citations from `scanned` files that the scan did not find again (`found` holds entry index and file), mark entries cited nowhere `stale`, and set a stale entry cited again back to `pending`.
    /// Returns the number of stale entries.
    pub(crate) fn mark_stale(
        &mut self,
        scanned: &HashSet<String>,
        found: &HashSet<(usize, String)>,
    ) -> usize {
        let mut stale = 0;
        for (idx, reference) in self.refs.iter_mut().enumerate() {
            reference
                .cited_in
                .retain(|file| !scanned.contains(file) || found.contains(&(idx, file.clone())));
            if reference.cited_in.is_empty() {
                reference.status = Status::Stale;
                stale += 1;
            } else if reference.status == Status::Stale {
                reference.status = Status::Pending;
            }
        }
        stale
    }

    /// The entries, sorted by URL
    pub(crate) fn into_sorted(mut self) -> Vec<Reference> {
        self.refs.sort_by(|a, b| a.url.cmp(&b.url));
        self.refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(url: &str, title: &str, cited_in: &[&str], status: Status) -> Reference {
        Reference {
            url: url.to_string(),
            title: title.to_string(),
            categories: vec!["general".to_string()],
            cited_in: cited_in.iter().map(ToString::to_string).collect(),
            status,
            verified: None,
            notes: None,
        }
    }

    #[test]
    fn spellings_of_one_url_share_a_key() {
        let key = url_key("https://example.com/a");
        for same in [
            "HTTPS://Example.COM/a",
            "https://example.com:443/a",
            "https://example.com/a/",
            "https://example.com/a#part",
            "https://example.com/a?utm_source=x&UTM_Medium=y",
            "https://example.com/a?&utm_campaign=z",
        ] {
            assert_eq!(url_key(same), key, "{same}");
        }
        assert_eq!(
            url_key("https://example.com/"),
            url_key("https://example.com")
        );
    }

    #[test]
    fn different_urls_keep_different_keys() {
        let key = url_key("https://example.com/a");
        for other in [
            "http://example.com/a",
            "https://www.example.com/a",
            "https://example.com/A",
            "https://example.com:8443/a",
            "https://example.com/a?page=2",
        ] {
            assert_ne!(url_key(other), key, "{other}");
        }
        // Other parameters are kept in order and as written
        assert_eq!(
            url_key("https://example.com/a?b=2&utm_x=1&a=1"),
            "https://example.com/a?b=2&a=1"
        );
        assert_eq!(url_key("not a url"), "not a url");
    }

    #[test]
    fn the_worse_status_ranks_higher() {
        let order = [
            Status::Stale,
            Status::Ok,
            Status::Pending,
            Status::Paywall,
            Status::Login,
            Status::Redirect,
            Status::Dead,
        ];
        let ranks: Vec<u8> = order.iter().map(|s| severity(*s)).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
    }

    #[test]
    fn duplicates_in_a_file_merge_into_the_first() {
        let mut dead = reference("https://example.com/a/", "A", &["b.md"], Status::Dead);
        dead.notes = Some("HTTP 404".to_string());
        dead.categories = vec!["research".to_string()];
        let set = RefSet::new(vec![
            reference(
                "https://example.com/a",
                "https://example.com/a",
                &["a.md"],
                Status::Ok,
            ),
            dead,
            reference(
                "https://example.com/a#x",
                "Other",
                &["a.md"],
                Status::Pending,
            ),
            reference("https://example.com/b", "B", &["a.md"], Status::Ok),
        ]);
        assert_eq!((set.len(), set.merged()), (2, 2));
        let refs = set.into_sorted();
        let a = &refs[0];
        assert_eq!(a.url, "https://example.com/a", "the first spelling is kept");
        assert_eq!(
            a.title, "A",
            "a real title beats a bare URL, and the first one wins"
        );
        assert_eq!(a.cited_in, ["a.md", "b.md"]);
        assert_eq!(a.categories, ["general", "research"]);
        assert_eq!(
            (a.status, a.notes.as_deref()),
            (Status::Dead, Some("HTTP 404"))
        );
    }

    #[test]
    fn finding_a_url_notes_only_a_different_spelling() {
        let mut set = RefSet::new(vec![reference(
            "https://example.com/a",
            "A",
            &["a.md"],
            Status::Ok,
        )]);
        assert_eq!(set.find("https://example.com/a"), Some(0));
        assert_eq!(set.merged(), 0);
        assert_eq!(set.find("https://example.com/a/"), Some(0));
        assert_eq!(set.merged(), 1);
        assert_eq!(set.find("https://example.com/b"), None);
        let idx = set.push(reference(
            "https://example.com/b",
            "B",
            &[],
            Status::Pending,
        ));
        set.get_mut(idx).title = "Bee".to_string();
        assert_eq!(set.into_sorted()[1].title, "Bee");
    }

    #[test]
    fn citations_the_scan_did_not_find_again_are_dropped_and_uncited_entries_go_stale() {
        let mut set = RefSet::new(vec![
            // Cited in a scanned file that still cites it, and in one not scanned
            reference(
                "https://example.com/kept",
                "K",
                &["a.md", "other.md"],
                Status::Ok,
            ),
            // Cited only in a scanned file that no longer cites it
            reference("https://example.com/gone", "G", &["a.md"], Status::Ok),
            // Stale, now cited again
            reference("https://example.com/back", "B", &["a.md"], Status::Stale),
        ]);
        let scanned: HashSet<String> = ["a.md".to_string()].into();
        let found: HashSet<(usize, String)> =
            [(0, "a.md".to_string()), (2, "a.md".to_string())].into();
        assert_eq!(set.mark_stale(&scanned, &found), 1);
        let refs = set.into_sorted();
        let by_url = |url: &str| refs.iter().find(|r| r.url.ends_with(url)).unwrap();
        assert_eq!(by_url("/kept").cited_in, ["a.md", "other.md"]);
        assert_eq!(by_url("/kept").status, Status::Ok);
        assert_eq!(by_url("/gone").cited_in, Vec::<String>::new());
        assert_eq!(by_url("/gone").status, Status::Stale);
        assert_eq!(by_url("/back").status, Status::Pending);
    }
}
