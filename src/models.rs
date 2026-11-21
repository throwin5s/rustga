//! Data model shared by every source.
//!
//! Field names follow Mihon's `SManga`, `SChapter` and `Page` so a Keiyoushi
//! extension translates one-to-one, with two Rust-isms: unknown values are
//! `Option` instead of sentinel strings, and `MangaStatus` is an enum instead of
//! an int.

use serde::{Deserialize, Serialize};

/// Publication state of a series, matching Mihon's `SManga` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MangaStatus {
    /// The source did not say.
    #[default]
    Unknown,
    Ongoing,
    Completed,
    /// Officially licensed in the source's region.
    Licensed,
    /// Finished publishing, as reported by the site.
    PublishingFinished,
    Cancelled,
    OnHiatus,
}

/// A series as a source sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manga {
    /// Source-relative or absolute URL that identifies the series. Stored in
    /// the library, so it must stay stable for a given series.
    pub url: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    /// Comma-separated genres, mirroring Mihon's single `genre` field.
    pub genre: Option<String>,
    pub status: MangaStatus,
}

impl Manga {
    /// A listed series: the minimum a source must fill in.
    pub fn new(url: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            ..Self::default()
        }
    }
}

/// One chapter of a series.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    /// Source-relative or absolute URL that identifies the chapter.
    pub url: String,
    /// Display name, e.g. `Chapter 12.5 - Extra`.
    pub name: String,
    /// Parsed chapter number, when the site gives one.
    pub chapter_number: Option<f32>,
    pub scanlator: Option<String>,
    /// Upload time as a Unix timestamp in seconds; `0` when unknown.
    pub uploaded_at: i64,
}

/// One image of a chapter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// Position in the chapter, starting at zero.
    pub index: usize,
    /// Page identifier returned by the source, often the image URL itself.
    pub url: String,
    /// Image URL when it differs from `url`, e.g. because it needs a lookup.
    pub image_url: Option<String>,
}

/// One page of a listing, mirroring Mihon's `MangasPage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MangaPage {
    pub mangas: Vec<Manga>,
    /// Whether asking for `page + 1` is worthwhile.
    pub has_next_page: bool,
}

impl MangaPage {
    pub fn new(mangas: Vec<Manga>, has_next_page: bool) -> Self {
        Self {
            mangas,
            has_next_page,
        }
    }

    /// A single, final page — the common case for search results.
    pub fn single(mangas: Vec<Manga>) -> Self {
        Self::new(mangas, false)
    }
}
