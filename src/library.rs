//! Saved series: the list the user builds while browsing or scanning a thread.
//!
//! Kept apart from [`crate::downloads`], which records chapter files on disk. An
//! entry points at its source by the stable id from [`crate::source::derive_id`],
//! so a series stays attached to the site it came from. The identity of an entry
//! is the pair `(source, manga.url)`; the title is only a label.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::local;
use crate::models::Manga;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Stable id of the source that owns the series.
    pub source: u64,
    pub manga: Manga,
    /// Unix seconds; `0` when the file predates the field.
    #[serde(default)]
    pub added_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub entries: Vec<LibraryEntry>,
}

impl Library {
    pub fn path(root: &Path) -> PathBuf {
        root.join("library.json")
    }

    /// A missing or unreadable file is an empty library, not a failed launch.
    pub fn load(root: &Path) -> Self {
        local::read(&Self::path(root))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> io::Result<()> {
        local::write(&Self::path(root), self)
    }

    pub fn contains(&self, source: u64, url: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.source == source && entry.manga.url == url)
    }

    /// Adds a series once. Returns false when the same source already lists it
    /// or the entry has no URL to open it with.
    pub fn add(&mut self, source: u64, manga: &Manga) -> bool {
        if manga.url.is_empty() || self.contains(source, &manga.url) {
            return false;
        }
        self.entries.push(LibraryEntry {
            source,
            manga: manga.clone(),
            added_at: now(),
        });
        true
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        PathBuf::from(format!(
            "target/cache-test-library-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn saves_once_per_source_and_survives_a_reload() {
        let root = scratch("round-trip");
        let mut library = Library::default();
        let manga = Manga::new("/manga/1", "One");

        assert!(library.add(7, &manga));
        assert!(
            !library.add(7, &manga),
            "the same series is not added twice"
        );
        assert!(
            library.add(9, &manga),
            "another source may list the same URL"
        );

        library.save(&root).unwrap();
        let reloaded = Library::load(&root);
        assert_eq!(reloaded.entries.len(), 2);
        assert!(reloaded.contains(7, "/manga/1"));
        assert!(reloaded.entries[0].added_at > 0);

        // A series without a URL cannot be opened, so it is not kept.
        assert!(!library.add(7, &Manga::new("", "No URL")));

        let _ = std::fs::remove_file(Library::path(&root));
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn a_missing_file_is_an_empty_library() {
        let root = scratch("missing");
        assert!(Library::load(&root).entries.is_empty());
    }
}
