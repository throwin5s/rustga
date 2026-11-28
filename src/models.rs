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
    /// Additional source-provided tags, when available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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

/// `2026-11-20 04:23:36` or `2026-11-20T04:23:36.12Z` as Unix seconds, or
/// `0` when the text is not a date. Both sources publish UTC, so a trailing
/// offset is ignored.
pub fn parse_timestamp(text: &str) -> i64 {
    let field = |start: usize, end: usize| -> Option<i64> { text.get(start..end)?.parse().ok() };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        field(0, 4),
        field(5, 7),
        field(8, 10),
        field(11, 13),
        field(14, 16),
        field(17, 19),
    ) else {
        return 0;
    };
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    // Days from the civil date, counting from 1970-01-01.
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400 + hour * 3_600 + minute * 60 + second
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
    fn timestamps_parse_from_each_source_format() {
        assert_eq!(
            parse_timestamp("2026-11-20 04:23:36.633674+00"),
            1_795_148_616
        );
        assert_eq!(parse_timestamp("2026-11-28T02:49:21.549000"), 1_795_834_161);
        assert_eq!(parse_timestamp("not a date"), 0);
        assert_eq!(parse_timestamp(""), 0);
    }

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
