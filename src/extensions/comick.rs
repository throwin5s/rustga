//! Comick — <https://comick.dev>, the site formerly at comick.io.
//!
//! Browse-only source. Comick's public catalog answers listing and search
//! requests, but `/v1.0/comic/{slug}/` and the chapter routes sit behind a
//! Cloudflare challenge for clients without a browser session, so the app
//! cannot open a series or list its chapters. [`Source::browse_only`] tells the
//! host to open these titles on comick.dev instead.
//!
//! Endpoints in use, from the official explorer at <https://api.comick.dev/docs/>:
//!
//! - `GET /top?media_type=manga&accept_mature_content=false` — homepage feeds.
//!   `trending`, `topFollowComics` and `topFollowNewComics` are objects keyed by
//!   window in days (`7`, `30`, `90`); this source reads the 7-day window. The
//!   shelf titles match the site's own labels: Most Recent Popular, Most
//!   Followed and Popular New Comics.
//!
//! The website hides rows without a demographic for readers who set a gender
//! preference, so its shelf is slightly shorter than the raw feed. The same
//! filter is applied here, otherwise the app shows a title the site leaves out.
//! - `GET /v1.0/search/?q=&page=&limit=` — public catalog search.
//!
//! Covers are `https://meo.comick.pictures/{b2key}`; a title's page is
//! `https://comick.dev/comic/{slug}`. Feed rows omit author, description and
//! status, so shelves show only what they carry; search rows carry more.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::filters::FilterValues;
use crate::http::HttpSource;
use crate::models::{Chapter, Manga, MangaPage, MangaSection, MangaStatus, Page};
use crate::source::{Source, SourceInfo};

const BASE_URL: &str = "https://comick.dev";
const API_URL: &str = "https://api.comick.dev";

/// Search page size; the API caps pages at 50 results and 50 pages.
const SEARCH_LIMIT: i64 = 30;

/// The catalog feeds are edge-cached and cheap; stay polite all the same.
const RATE_LIMIT_MS: u64 = 300;

const LANGUAGE: &str = "en";

/// The ranked window every shelf reads.
const WINDOW: &str = "7";

pub struct Comick {
    info: SourceInfo,
    http: HttpSource,
}

impl Comick {
    pub fn new() -> Result<Self> {
        Ok(Self {
            info: SourceInfo::new("Comick", LANGUAGE, 1, BASE_URL),
            http: HttpSource::new(RATE_LIMIT_MS)?
                .with_header("Referer", &format!("{BASE_URL}/"))?,
        })
    }

    /// One request for every homepage feed.
    fn top(&self) -> Result<Value> {
        self.http.get_json(&format!(
            "{API_URL}/top?media_type=manga&accept_mature_content=false"
        ))
    }
}

impl Source for Comick {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn browse_only(&self) -> bool {
        true
    }

    /// The three shelves the Browse screen shows, in display order.
    fn sections(&self) -> Result<Vec<MangaSection>> {
        Ok(sections_from(&self.top()?))
    }

    fn popular(&self, page: i64) -> Result<MangaPage> {
        // The feed has no pagination; asking for another page would repeat it.
        if page > 1 {
            return Ok(MangaPage::single(Vec::new()));
        }
        Ok(MangaPage::single(feed_rows(&self.top()?, "trending")))
    }

    fn search(&self, query: &str, page: i64, _filters: &FilterValues) -> Result<MangaPage> {
        let query = query.trim();
        if query.is_empty() {
            return self.popular(page);
        }
        let page = page.max(1);
        let mut url = reqwest::Url::parse(&format!("{API_URL}/v1.0/search/"))
            .map_err(|error| Error::Url(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("page", &page.to_string())
            .append_pair("limit", &SEARCH_LIMIT.to_string());
        search_from(&self.http.get_json(url.as_str())?, page)
    }

    /// Listings already hold everything Comick exposes without a browser
    /// session, so there is nothing to fetch.
    fn details(&self, manga: &Manga) -> Result<Manga> {
        Ok(manga.clone())
    }

    fn chapters(&self, _manga: &Manga) -> Result<Vec<Chapter>> {
        Err(Error::Unsupported(
            "Comick chapters are only available on comick.dev",
        ))
    }

    fn pages(&self, _chapter: &Chapter) -> Result<Vec<Page>> {
        Err(Error::Unsupported(
            "Comick chapters are only available on comick.dev",
        ))
    }
}

/// The three shelves, skipping any feed the response omits or empties.
fn sections_from(body: &Value) -> Vec<MangaSection> {
    [
        ("trending", "Most Recent Popular"),
        ("topFollowComics", "Most Followed"),
        ("topFollowNewComics", "Popular New Comics"),
    ]
    .into_iter()
    .filter_map(|(key, title)| {
        let mangas = feed_rows(body, key);
        (!mangas.is_empty()).then(|| MangaSection {
            title: title.to_owned(),
            mangas,
        })
    })
    .collect()
}

fn feed_rows(body: &Value, key: &str) -> Vec<Manga> {
    let Some(feed) = body.get(key) else {
        return Vec::new();
    };
    // With `type` and `day` the feed is a bare array; the homepage payload
    // keys every feed by window.
    let rows = feed
        .get(WINDOW)
        .and_then(Value::as_array)
        .or_else(|| feed.as_array());
    rows.map(|rows| {
        rows.iter()
            .filter(|row| has_demographic(row))
            .filter_map(manga_from_row)
            .collect()
    })
    .unwrap_or_default()
}

/// The website hides catalogue rows without a demographic when the reader has
/// gender preferences set; mirror that so both shelves match.
fn has_demographic(row: &Value) -> bool {
    row.get("demographic")
        .and_then(Value::as_u64)
        .is_some_and(|demographic| demographic != 0)
}

fn search_from(body: &Value, page: i64) -> Result<MangaPage> {
    let rows = body
        .as_array()
        .ok_or_else(|| Error::Parse("search reply is not an array".to_owned()))?;
    let mangas = rows.iter().filter_map(manga_from_row).collect();
    let has_next = rows.len() as i64 >= SEARCH_LIMIT && page < 50;
    Ok(MangaPage::new(mangas, has_next))
}

/// One catalog row, from either a feed or a search reply.
fn manga_from_row(row: &Value) -> Option<Manga> {
    let slug = row
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| !slug.is_empty())?;
    let title = text(row, "title")?;

    let mut manga = Manga::new(format!("{BASE_URL}/comic/{slug}"), title.clone());
    manga.thumbnail_url = cover_url(row);
    manga.description = text(row, "desc");
    manga.genre = genre_names(row);
    manga.status = status(row.get("status"));
    manga.year = year(row);
    manga.demographic = demographic(row.get("demographic"));
    manga.alt_titles = alt_titles(row, &title);
    Some(manga)
}

/// `meo.comick.pictures` is the stable cover host; `gpurl` is a fallback.
fn cover_url(row: &Value) -> Option<String> {
    let covers = row.get("md_covers").and_then(Value::as_array)?;
    let cover = covers.first()?;
    if let Some(key) = cover.get("b2key").and_then(Value::as_str) {
        if !key.is_empty() {
            return Some(format!("https://meo.comick.pictures/{key}"));
        }
    }
    cover
        .get("gpurl")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned)
}

/// Genre names from the joined rows, preserving the site's order.
fn genre_names(row: &Value) -> Option<String> {
    let rows = row.get("md_comic_md_genres")?.as_array()?;
    let mut names = Vec::new();
    for genre in rows {
        let Some(name) = genre
            .get("md_genres")
            .and_then(|genre| genre.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !names
            .iter()
            .any(|seen: &String| seen.eq_ignore_ascii_case(name))
        {
            names.push(name.to_owned());
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// Comick numbers demographics: 1 shounen, 2 shoujo, 3 seinen, 4 josei.
fn demographic(value: Option<&Value>) -> Option<String> {
    match value.and_then(Value::as_u64)? {
        1 => Some("Shounen".to_owned()),
        2 => Some("Shoujo".to_owned()),
        3 => Some("Seinen".to_owned()),
        4 => Some("Josei".to_owned()),
        _ => None,
    }
}

/// Comick's publication codes: 1 ongoing, 2 completed, 3 cancelled, 4 hiatus.
fn status(value: Option<&Value>) -> MangaStatus {
    match value.and_then(Value::as_u64) {
        Some(1) => MangaStatus::Ongoing,
        Some(2) => MangaStatus::Completed,
        Some(3) => MangaStatus::Cancelled,
        Some(4) => MangaStatus::OnHiatus,
        _ => MangaStatus::Unknown,
    }
}

fn year(row: &Value) -> Option<u16> {
    row.get("year")
        .and_then(Value::as_u64)
        .and_then(|year| u16::try_from(year).ok())
        .filter(|year| (1900..=2100).contains(year))
}

/// Alternate names from `md_titles`, minus the display title and repeats.
fn alt_titles(row: &Value, title: &str) -> Vec<String> {
    let mut titles: Vec<String> = row
        .get("md_titles")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(|row| text(row, "title")).collect())
        .unwrap_or_default();
    titles.retain(|alt| !alt.eq_ignore_ascii_case(title));
    titles.sort_by_key(|alt| alt.to_lowercase());
    titles.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    titles
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::json!({
            "trending": {"7": [{
                "slug": "invincible-mumu",
                "title": "Invincible Mumu",
                "demographic": 1,
                "md_comic_md_genres": [
                    {"md_genres": {"name": "Shounen"}},
                    {"md_genres": {"name": "Action"}}
                ],
                "md_covers": [{"b2key": "Krm1K5.jpg"}]
            }]},
            "topFollowComics": {"7": [{
                "slug": "solo-leveling",
                "title": "Solo Leveling",
                "demographic": 3,
                "md_covers": [{"b2key": "cover.jpg"}]
            }]},
            "topFollowNewComics": {"7": [{
                "slug": "new-title",
                "title": "New Title",
                "demographic": 2,
                "md_covers": [{"gpurl": "https://tmp2.comick.pictures/new.jpg"}]
            }]}
        })
    }

    #[test]
    fn reads_the_three_shelves_in_order() {
        let sections = sections_from(&fixture());
        assert_eq!(
            sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Most Recent Popular", "Most Followed", "Popular New Comics"]
        );

        let trending = &sections[0].mangas[0];
        assert_eq!(trending.title, "Invincible Mumu");
        assert_eq!(trending.url, "https://comick.dev/comic/invincible-mumu");
        assert_eq!(
            trending.thumbnail_url.as_deref(),
            Some("https://meo.comick.pictures/Krm1K5.jpg")
        );
        assert_eq!(trending.genre.as_deref(), Some("Shounen, Action"));
        assert_eq!(trending.demographic.as_deref(), Some("Shounen"));

        assert_eq!(
            sections[2].mangas[0].thumbnail_url.as_deref(),
            Some("https://tmp2.comick.pictures/new.jpg")
        );
    }

    #[test]
    fn reads_search_metadata_and_pages() {
        let body = serde_json::json!([
            {
                "slug": "00-solo",
                "title": "Solo",
                "desc": "A description.",
                "status": 2,
                "year": 2021,
                "demographic": 3,
                "md_covers": [{"b2key": "eBMOo.jpg"}],
                "md_titles": [
                    {"title": "Solo Leveling", "lang": "en"},
                    {"title": "Only I Level Up", "lang": "en"},
                    {"title": "Solo", "lang": "en"}
                ]
            }
        ]);
        let page = search_from(&body, 1).unwrap();
        assert!(!page.has_next_page);
        let manga = &page.mangas[0];
        assert_eq!(manga.title, "Solo");
        assert_eq!(manga.description.as_deref(), Some("A description."));
        assert_eq!(manga.status, MangaStatus::Completed);
        assert_eq!(manga.year, Some(2021));
        assert_eq!(manga.demographic.as_deref(), Some("Seinen"));
        assert_eq!(manga.alt_titles, ["Only I Level Up", "Solo Leveling"]);
    }

    #[test]
    fn rows_without_a_demographic_are_hidden_like_the_website() {
        let body = serde_json::json!({
            "trending": {"7": [
                {"slug": "kept", "title": "Kept", "demographic": 1,
                 "md_covers": [{"b2key": "a.jpg"}]},
                {"slug": "unset", "title": "Unset", "demographic": null,
                 "md_covers": [{"b2key": "b.jpg"}]},
                {"slug": "zero", "title": "Zero", "demographic": 0,
                 "md_covers": [{"b2key": "c.jpg"}]}
            ]}
        });
        let mangas = feed_rows(&body, "trending");
        assert_eq!(mangas.len(), 1);
        assert_eq!(mangas[0].title, "Kept");
        assert_eq!(mangas[0].demographic.as_deref(), Some("Shounen"));
    }

    #[test]
    fn unknown_rows_are_skipped_and_status_codes_map() {
        assert!(manga_from_row(&serde_json::json!({"title": "No slug"})).is_none());
        assert_eq!(status(Some(&serde_json::json!(1))), MangaStatus::Ongoing);
        assert_eq!(status(Some(&serde_json::json!(4))), MangaStatus::OnHiatus);
        assert_eq!(status(None), MangaStatus::Unknown);
    }
}
