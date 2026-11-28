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

impl MangaStatus {
    /// Display label, or `None` when the source did not say.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::Ongoing => Some("Ongoing"),
            Self::Completed => Some("Completed"),
            Self::Licensed => Some("Licensed"),
            Self::PublishingFinished => Some("Finished"),
            Self::Cancelled => Some("Cancelled"),
            Self::OnHiatus => Some("On hiatus"),
        }
    }
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
    /// Shounen, Shoujo, Seinen or Josei, when the source states one.
    #[serde(default)]
    pub demographic: Option<String>,
    /// Year the series first ran.
    #[serde(default)]
    pub year: Option<u16>,
    /// Year the series stopped, when the run has finished.
    #[serde(default)]
    pub end_year: Option<u16>,
    /// Native, romanized and alternate names, used to match a title a thread
    /// named against what the source calls it.
    #[serde(default)]
    pub alt_titles: Vec<String>,
}

impl Manga {
    /// `2008–2011` for a finished run, `2008` when only the start is known.
    pub fn years(&self) -> Option<String> {
        match (self.year, self.end_year) {
            (Some(start), Some(end)) if end > start => Some(format!("{start}–{end}")),
            (Some(start), _) => Some(start.to_string()),
            _ => None,
        }
    }

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

/// A named row of titles, for sites that publish several homepage feeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MangaSection {
    pub title: String,
    pub mangas: Vec<Manga>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn years_read_as_a_range_only_when_the_run_has_ended() {
        let mut manga = Manga::new("/m/1", "Example");
        assert_eq!(manga.years(), None);
        manga.year = Some(2008);
        assert_eq!(manga.years().as_deref(), Some("2008"));
        manga.end_year = Some(2011);
        assert_eq!(manga.years().as_deref(), Some("2008–2011"));
        manga.end_year = Some(2008);
        assert_eq!(manga.years().as_deref(), Some("2008"));
    }

    /// The library file predates `end_year`, so its rows must still load.
    #[test]
    fn a_row_saved_before_end_years_still_loads() {
        let mut value = serde_json::to_value(Manga::new("/m/1", "Example")).expect("serialize");
        value
            .as_object_mut()
            .expect("manga object")
            .remove("end_year");
        let manga: Manga = serde_json::from_value(value).expect("deserialize");
        assert_eq!(manga.end_year, None);
    }
}
