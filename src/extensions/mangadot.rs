//! MangaDot — <https://mangadot.net>.
//!
//! Hybrid source: the site's own JSON API for search, series details and
//! chapters, its server-rendered HTML for listings and reader pages. No
//! Keiyoushi extension exists for MangaDot, so every endpoint below was found on
//! the live site rather than borrowed.
//!
//! Endpoints in use:
//!
//! - `GET /api/search?search=&limit=&page=` — JSON, `manga_list` + `pagination`
//! - `GET /api/manga/{id}` — JSON, full series metadata
//! - `GET /api/manga/{id}/chapters/list` — JSON, every release in one response
//! - `GET /view-all/{route}?page=` — SSR cards, 28 per page
//! - `GET /api/chapters/{id}/images` — ordered page manifest for one chapter
//!
//! ## One request per screen, like a Mihon extension
//!
//! Chapters come from `/api/manga/{id}/chapters/list`, which answers a plain
//! array of every release (2169 rows for *Omniscient Reader*, 1236 of them
//! English) in about a second with no account. The neighbouring
//! `/api/manga/{id}/chapters` is the logged-in reader's endpoint and answers
//! `401 auth_required`; it is not the list endpoint. Nothing here walks chapter
//! pages, so a series opens as fast as its JSON downloads.

use std::collections::HashSet;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::filters::FilterValues;
use crate::http::{join_url, HttpSource};
use crate::models::{Chapter, Manga, MangaPage, MangaStatus, Page};
use crate::source::{Source, SourceInfo};

const BASE_URL: &str = "https://mangadot.net";

/// The API's own page size cap is 28; stay under it.
const SEARCH_LIMIT: i64 = 24;

/// Slow enough to stay unobtrusive on a site that has no published rate limit.
const RATE_LIMIT_MS: u64 = 250;

/// Languages the source surfaces. The site carries multi-language releases of
/// the same chapter behind the same number, so they must be filtered.
const LANGUAGE: &str = "en";

pub struct MangaDot {
    info: SourceInfo,
    http: HttpSource,
}

impl MangaDot {
    pub fn new() -> Result<Self> {
        Ok(Self {
            info: SourceInfo::new("MangaDot", LANGUAGE, 1, BASE_URL),
            http: HttpSource::new(RATE_LIMIT_MS)?
                .with_header("Referer", &format!("{BASE_URL}/"))?,
        })
    }

    fn absolute(&self, path: &str) -> Result<String> {
        join_url(BASE_URL, path)
    }

    fn listing(&self, route: &str, page: i64) -> Result<MangaPage> {
        let url = self.absolute(&format!("/view-all/{route}?page={}", page.max(1)))?;
        parse_cards(&self.http.get_text(&url)?)
    }

    fn search_url(query: &str, page: i64) -> Result<String> {
        let mut url = reqwest::Url::parse(&format!("{BASE_URL}/api/search"))
            .map_err(|error| Error::Url(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("search", query)
            .append_pair("limit", &SEARCH_LIMIT.to_string())
            .append_pair("page", &page.max(1).to_string());
        Ok(url.to_string())
    }
}

impl Source for MangaDot {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn supports_latest(&self) -> bool {
        true
    }

    fn popular(&self, page: i64) -> Result<MangaPage> {
        self.listing("most-tracked", page)
    }

    fn latest(&self, page: i64) -> Result<MangaPage> {
        self.listing("latest-updates", page)
    }

    fn search(&self, query: &str, page: i64, _filters: &FilterValues) -> Result<MangaPage> {
        if query.trim().is_empty() {
            return self.popular(page);
        }
        let body = self.http.get_json(&Self::search_url(query, page)?)?;
        parse_search(&body)
    }

    fn details(&self, manga: &Manga) -> Result<Manga> {
        let id = manga_id(&manga.url).ok_or_else(|| Error::NotFound(manga.url.clone()))?;
        let body = self
            .http
            .get_json(&self.absolute(&format!("/api/manga/{id}"))?)?;
        let data = body
            .get("manga")
            .ok_or_else(|| Error::Parse("missing manga object".to_owned()))?;

        let mut detailed = manga.clone();
        if let Some(title) = text_field(data, "title") {
            detailed.title = title;
        }
        detailed.description = text_field(data, "description");
        detailed.author = list_field(data, "authors");
        detailed.artist = list_field(data, "artists");
        detailed.genre = json_strings(data.get("genres"));
        detailed.thumbnail_url = text_field(data, "photo")
            .and_then(|photo| self.absolute(&photo).ok())
            .or(detailed.thumbnail_url);
        detailed.status = data
            .get("status")
            .and_then(Value::as_str)
            .map(status)
            .unwrap_or_default();
        Ok(detailed)
    }

    fn chapters(&self, manga: &Manga) -> Result<Vec<Chapter>> {
        let id = manga_id(&manga.url).ok_or_else(|| Error::NotFound(manga.url.clone()))?;
        let body = self
            .http
            .get_json(&self.absolute(&format!("/api/manga/{id}/chapters/list"))?)?;

        let mut chapters = parse_chapter_list(&body, LANGUAGE)?;
        chapters.sort_by(|left, right| {
            chapter_key(right)
                .total_cmp(&chapter_key(left))
                .then_with(|| left.url.cmp(&right.url))
        });
        Ok(chapters)
    }

    fn pages(&self, chapter: &Chapter) -> Result<Vec<Page>> {
        let id = chapter_id(&chapter.url).ok_or_else(|| Error::NotFound(chapter.url.clone()))?;
        let uploaded = reqwest::Url::parse(&chapter.url).ok().is_some_and(|url| {
            url.query_pairs()
                .any(|(key, value)| key == "source" && value == "user")
        });
        let route = if uploaded { "uploads" } else { "chapters" };
        let body = self
            .http
            .get_json(&self.absolute(&format!("/api/{route}/{id}/images"))?)?;
        let images = parse_page_manifest(&body)?;
        let pages = images
            .iter()
            .enumerate()
            .map(|(index, url)| {
                let absolute = self.absolute(url)?;
                Ok(Page {
                    index,
                    url: absolute,
                    image_url: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if pages.is_empty() {
            return Err(Error::Parse(format!("no pages found for {}", chapter.url)));
        }
        Ok(pages)
    }
}

/// `https://mangadot.net/manga/4840` (or the bare path) to `4840`.
fn manga_id(url: &str) -> Option<u64> {
    let rest = url.split("/manga/").nth(1)?;
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

fn chapter_id(url: &str) -> Option<u64> {
    let rest = url.split("/chapter/").nth(1)?;
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

/// Number for ordering; chapters the site gives no number for sort last.
fn chapter_key(chapter: &Chapter) -> f32 {
    chapter.chapter_number.unwrap_or(f32::MIN)
}

/// Every release of one series, filtered to one language.
///
/// `/chapters/list` returns all languages and every group's version of the same
/// chapter number; deduplication is the host's job, so this keeps them all.
fn parse_chapter_list(body: &Value, language: &str) -> Result<Vec<Chapter>> {
    let rows = body
        .as_array()
        .ok_or_else(|| Error::Parse("chapter list is not an array".to_owned()))?;

    let mut chapters = Vec::new();
    for row in rows {
        if row.get("language").and_then(Value::as_str) != Some(language) {
            continue;
        }
        let Some(id) = row.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let number = row
            .get("chapter_number")
            .and_then(Value::as_f64)
            .map(|number| number as f32);
        let source = row
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("scraper");
        let name = text_field(row, "chapter_title").unwrap_or_else(|| match number {
            Some(number) => format!("Chapter {number}"),
            None => "Chapter".to_owned(),
        });

        chapters.push(Chapter {
            url: format!("{BASE_URL}/chapter/{id}?source={source}"),
            name,
            chapter_number: number,
            scanlator: text_field(row, "group_name"),
            // The list ships a timestamp, but nothing sorts by it yet.
            uploaded_at: 0,
        });
    }

    Ok(chapters)
}

/// Listing cards: `a[href^="/manga/"]` wrapping a cover and a `[data-tl-en]`
/// title span. `/view-all` pages are server-rendered, 28 cards each.
fn parse_cards(body: &str) -> Result<MangaPage> {
    let document = Html::parse_document(body);
    let card = selector("a[href^=\"/manga/\"]");
    let title = selector("[data-tl-en]");
    let fallback = selector("div[class*=\"line-clamp-2\"]");
    let cover = selector("img");

    let mut mangas = Vec::new();
    let mut seen = HashSet::new();

    for element in document.select(&card) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        let Some(id) = manga_id(href) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }

        let name = element
            .select(&title)
            .next()
            .map(text_of)
            .or_else(|| element.select(&fallback).next().map(text_of))
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }

        let mut manga = Manga::new(join_url(BASE_URL, href)?, name);
        manga.thumbnail_url = element
            .select(&cover)
            .next()
            .and_then(|image| image.value().attr("src"))
            .and_then(|src| join_url(BASE_URL, src).ok());
        mangas.push(manga);
    }

    // An empty page means the listing is exhausted; the site does not say so itself.
    Ok(MangaPage::new(mangas.clone(), !mangas.is_empty()))
}

/// `/api/search` payload.
fn parse_search(body: &Value) -> Result<MangaPage> {
    let list = body
        .get("manga_list")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse("missing manga_list".to_owned()))?;

    let mut mangas = Vec::with_capacity(list.len());
    for item in list {
        let Some(id) = item.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(title) = text_field(item, "title") else {
            continue;
        };

        let mut manga = Manga::new(join_url(BASE_URL, &format!("/manga/{id}"))?, title);
        manga.thumbnail_url =
            text_field(item, "photo").and_then(|photo| join_url(BASE_URL, &photo).ok());
        manga.genre = json_strings(item.get("genres"));
        manga.status = item
            .get("status")
            .and_then(Value::as_str)
            .map(status)
            .unwrap_or_default();
        mangas.push(manga);
    }

    let has_next_page = body
        .pointer("/pagination/current_page")
        .and_then(Value::as_i64)
        .zip(
            body.pointer("/pagination/total_pages")
                .and_then(Value::as_i64),
        )
        .map(|(current, total)| current < total)
        .unwrap_or(!mangas.is_empty());

    Ok(MangaPage::new(mangas, has_next_page))
}

/// The reader API returns either URL strings or objects carrying a `url`.
fn parse_page_manifest(body: &Value) -> Result<Vec<String>> {
    let images = body
        .get("images")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse("missing page manifest images".to_owned()))?;
    let pages = images
        .iter()
        .filter_map(|image| {
            image
                .as_str()
                .or_else(|| image.get("url").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    if pages.is_empty() {
        return Err(Error::Parse("page manifest is empty".to_owned()));
    }
    Ok(pages)
}

fn text_of(element: ElementRef<'_>) -> String {
    crate::html::normalize(&element.text().collect::<Vec<_>>().join(" "))
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Array-of-strings field joined for display, e.g. `genres`.
fn json_strings(value: Option<&Value>) -> Option<String> {
    let array = value?.as_array()?;
    let joined = array
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    Some(joined).filter(|joined| !joined.is_empty())
}

/// `authors` / `artists` arrive as a JSON-encoded string, though arrays are
/// handled too in case the API changes.
fn list_field(value: &Value, key: &str) -> Option<String> {
    let raw = value.get(key)?;
    if let Some(array) = raw.as_array() {
        let joined = array
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(joined).filter(|joined| !joined.is_empty());
    }
    let encoded = raw.as_str()?;
    let decoded: Vec<String> = serde_json::from_str(encoded).ok()?;
    let joined = decoded.join(", ");
    Some(joined).filter(|joined| !joined.is_empty())
}

fn status(value: &str) -> MangaStatus {
    match value.to_ascii_lowercase().as_str() {
        "ongoing" => MangaStatus::Ongoing,
        "completed" => MangaStatus::Completed,
        "cancelled" | "canceled" => MangaStatus::Cancelled,
        "hiatus" | "on hiatus" => MangaStatus::OnHiatus,
        "licensed" => MangaStatus::Licensed,
        _ => MangaStatus::Unknown,
    }
}

fn selector(pattern: &str) -> Selector {
    Selector::parse(pattern).expect("static selector")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manga_ids() {
        assert_eq!(manga_id("https://mangadot.net/manga/4840"), Some(4840));
        assert_eq!(manga_id("/manga/175"), Some(175));
        assert_eq!(manga_id("/chapter/358858"), None);
    }

    #[test]
    fn keeps_one_language_and_every_release() {
        let body = serde_json::json!([
            {"id": 1, "chapter_number": 0, "language": "en", "chapter_title": "Prologue",
             "group_name": "Asura Scans", "source": "user"},
            {"id": 2, "chapter_number": 0, "language": "en", "chapter_title": "Chapter 0",
             "group_name": "Flame Comics", "source": "scraper"},
            {"id": 3, "chapter_number": 0, "language": "fr", "chapter_title": "Prologue",
             "group_name": "Anime-Sama", "source": "user"},
            {"id": 4, "chapter_number": 1, "language": "en", "chapter_title": "",
             "group_name": null, "source": "user"}
        ]);
        let chapters = parse_chapter_list(&body, "en").unwrap();
        assert_eq!(chapters.len(), 3);
        assert_eq!(
            chapters[0].url,
            "https://mangadot.net/chapter/1?source=user"
        );
        assert_eq!(chapters[0].scanlator.as_deref(), Some("Asura Scans"));
        assert_eq!(chapters[1].scanlator.as_deref(), Some("Flame Comics"));
        assert_eq!(chapters[2].name, "Chapter 1");
        assert_eq!(chapters[2].chapter_number, Some(1.0));
    }

    #[test]
    fn reads_pages_from_the_json_manifest_in_source_order() {
        let body = serde_json::json!({
            "images": [
                {"url": "/chapters/series/chapter/001.webp"},
                {"url": "/chapters/series/chapter/002.webp"}
            ]
        });
        assert_eq!(
            parse_page_manifest(&body).unwrap(),
            vec![
                "/chapters/series/chapter/001.webp",
                "/chapters/series/chapter/002.webp",
            ]
        );
    }

    #[test]
    fn reads_cards_from_a_listing_page() {
        let body = r#"<a class="group" href="/manga/175">
            <img src="/uploads/thumbs/320/cover.webp"/>
            <div class="line-clamp-2"><span><span data-tl-en="">Omniscient Reader</span>
            <span data-tl-show="na">전지적 독자 시점</span></span></div></a>
            <a class="group" href="/manga/175"><img src="/x.webp"/><div class="line-clamp-2">
            <span data-tl-en="">Omniscient Reader</span></div></a>"#;
        let page = parse_cards(body).unwrap();
        assert_eq!(page.mangas.len(), 1);
        assert_eq!(page.mangas[0].title, "Omniscient Reader");
        assert_eq!(
            page.mangas[0].thumbnail_url.as_deref(),
            Some("https://mangadot.net/uploads/thumbs/320/cover.webp")
        );
        assert!(page.has_next_page);
    }
}
