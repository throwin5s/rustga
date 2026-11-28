//! MangaBall — <https://mangaball.com>.
//!
//! Uses the site's public `/api/v1` endpoints for titles, chapters and reader
//! pages. The endpoints are also used by MangaBall's own web client.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::filters::FilterValues;
use crate::http::{join_url, HttpSource};
use crate::models::{parse_timestamp, Chapter, Manga, MangaPage, MangaStatus, Page};
use crate::source::{Source, SourceInfo};

const BASE_URL: &str = "https://mangaball.com";
// MangaBall serves its reader site on mangaball.com and its public JSON API on
// api.mangaball.com. Requests to /api/v1 on the site host now hit the web app
// challenge instead of returning JSON.
const API_URL: &str = "https://api.mangaball.com/api/v1/";
const COVER_URL: &str = "https://bulbasaur.poke-black-and-white.net/covers";
const RATE_LIMIT_MS: u64 = 250;
const PAGE_SIZE: i64 = 24;
const CHAPTER_PAGE_SIZE: i64 = 100;

pub struct MangaBall {
    info: SourceInfo,
    http: HttpSource,
}

impl MangaBall {
    pub fn new() -> Result<Self> {
        Ok(Self {
            info: SourceInfo::new("MangaBall", "en", 1, BASE_URL),
            http: HttpSource::new(RATE_LIMIT_MS)?
                .with_header("Referer", &format!("{BASE_URL}/"))?,
        })
    }

    fn api(&self, path: &str) -> Result<String> {
        join_url(API_URL, path.trim_start_matches('/'))
    }

    fn search_page(&self, keyword: &str, page: i64, sort_by: &str) -> Result<MangaPage> {
        let mut url = reqwest::Url::parse(&self.api("/title/search-advanced")?)
            .map_err(|error| Error::Url(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("keyword", keyword)
            .append_pair("page", &page.max(1).to_string())
            .append_pair("limit", &PAGE_SIZE.to_string())
            .append_pair("sort_by", sort_by)
            .append_pair("sort_order", "desc");
        parse_title_page(&self.http.get_json(url.as_str())?)
    }
}

impl Source for MangaBall {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn supports_latest(&self) -> bool {
        true
    }

    fn popular(&self, page: i64) -> Result<MangaPage> {
        self.search_page("", page, "view_count")
    }

    fn latest(&self, page: i64) -> Result<MangaPage> {
        self.search_page("", page, "updated_at")
    }

    fn search(&self, query: &str, page: i64, _filters: &FilterValues) -> Result<MangaPage> {
        self.search_page(query.trim(), page, "updated_at")
    }

    fn details(&self, manga: &Manga) -> Result<Manga> {
        let key = title_key(&manga.url).ok_or_else(|| Error::NotFound(manga.url.clone()))?;
        let response = self
            .http
            .get_json(&self.api(&format!("/title/detail/{key}"))?)?;
        let data = response
            .get("data")
            .ok_or_else(|| Error::Parse("missing title data".to_owned()))?;

        let mut detailed = manga.clone();
        detailed.title = string_field(data, "name").unwrap_or(detailed.title);
        detailed.description = text_field(data.get("description"));
        detailed.author = people_field(data.get("authors").or_else(|| data.get("author")));
        detailed.artist = people_field(data.get("artists"));
        detailed.genre = data
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter(|tag| tag.get("group").and_then(Value::as_str) == Some("genre"))
                    .filter_map(|tag| string_field(tag, "name"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|genres| !genres.is_empty());
        detailed.tags = additional_tags(data);
        detailed.thumbnail_url = cover_url(data).or(detailed.thumbnail_url);
        detailed.status = string_field(data, "status")
            .map(|status| parse_status(&status))
            .unwrap_or_default();
        detailed.demographic = demographic(
            string_field(data, "publicationDemographic").or_else(|| tag_group(data, "demographic")),
        );
        detailed.alt_titles = alternate_names(data);
        detailed.year = year_of(string_field(data, "date_published"));
        Ok(detailed)
    }

    fn chapters(&self, manga: &Manga) -> Result<Vec<Chapter>> {
        let id = title_id(&manga.url).ok_or_else(|| Error::NotFound(manga.url.clone()))?;
        let mut chapters = Vec::new();
        let mut page = 1;
        loop {
            let mut url = reqwest::Url::parse(&self.api("/title/chapter-listing")?)
                .map_err(|error| Error::Url(error.to_string()))?;
            url.query_pairs_mut()
                .append_pair("title_id", &id)
                .append_pair("lang", "en")
                .append_pair("page", &page.to_string())
                .append_pair("limit", &CHAPTER_PAGE_SIZE.to_string());
            let response = self.http.get_json(url.as_str())?;
            let rows = response
                .get("data")
                .and_then(|data| {
                    data.as_array()
                        .or_else(|| data.get("chapters").and_then(Value::as_array))
                        .or_else(|| data.get("all_chapters").and_then(Value::as_array))
                })
                .ok_or_else(|| Error::Parse("missing chapter data".to_owned()))?;
            chapters.extend(rows.iter().flat_map(parse_chapters));
            let total_pages = total_pages(&response, page);
            if page >= total_pages || rows.is_empty() {
                break;
            }
            page += 1;
        }
        chapters.sort_by(|a, b| {
            b.chapter_number
                .unwrap_or(f32::MIN)
                .total_cmp(&a.chapter_number.unwrap_or(f32::MIN))
                .then_with(|| a.url.cmp(&b.url))
        });
        Ok(chapters)
    }

    fn pages(&self, chapter: &Chapter) -> Result<Vec<Page>> {
        let id = chapter_id(&chapter.url).ok_or_else(|| Error::NotFound(chapter.url.clone()))?;
        let mut url = reqwest::Url::parse(&self.api("/chapter-detail")?)
            .map_err(|error| Error::Url(error.to_string()))?;
        url.query_pairs_mut().append_pair("chapter_id", &id);
        let response = self.http.get_json(url.as_str())?;
        let images = response
            .pointer("/data/chapter/pages")
            .or_else(|| response.pointer("/data/pages"))
            .or_else(|| response.get("pages"))
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Parse(format!("missing pages for {}", chapter.url)))?;
        let pages = images
            .iter()
            .filter_map(page_url)
            .enumerate()
            .map(|(index, image)| Page {
                index,
                url: image,
                image_url: None,
            })
            .collect::<Vec<_>>();
        if pages.is_empty() {
            return Err(Error::Parse(format!("no pages found for {}", chapter.url)));
        }
        Ok(pages)
    }

    /// The image CDN is hotlink-protected: without the site Referer this is a
    /// 403, so pages have to go through the source's own client.
    fn image(&self, page: &Page) -> Result<Vec<u8>> {
        let url = self.image_url(page)?;
        self.http.get_bytes(&url)
    }
}

fn parse_title_page(body: &Value) -> Result<MangaPage> {
    let rows = body
        .get("data")
        .and_then(|data| {
            data.as_array()
                .or_else(|| data.get("manga").and_then(Value::as_array))
                .or_else(|| data.get("titles").and_then(Value::as_array))
        })
        .ok_or_else(|| Error::Parse("missing title results".to_owned()))?;
    let mangas = rows.iter().filter_map(parse_title).collect::<Vec<_>>();
    let current_page = body
        .pointer("/pagination/page")
        .or_else(|| body.pointer("/pagination/current_page"))
        .and_then(Value::as_i64);
    let total_pages = body
        .pointer("/pagination/total_pages")
        .or_else(|| body.pointer("/pagination/last_page"))
        .and_then(Value::as_i64);
    let has_next_page = current_page
        .zip(total_pages)
        .map(|(current, total)| current < total)
        .unwrap_or(!mangas.is_empty());
    Ok(MangaPage::new(mangas, has_next_page))
}

/// MangaBall versions use either `page`/`total_pages` or
/// `current_page`/`last_page` for the same pagination metadata.
fn total_pages(body: &Value, fallback: i64) -> i64 {
    body.pointer("/pagination/total_pages")
        .or_else(|| body.pointer("/pagination/last_page"))
        .and_then(Value::as_i64)
        .unwrap_or(fallback)
}

fn page_url(image: &Value) -> Option<String> {
    image
        .as_str()
        .or_else(|| image.get("url").and_then(Value::as_str))
        .or_else(|| image.get("image_url").and_then(Value::as_str))
        .or_else(|| image.get("src").and_then(Value::as_str))
        .map(str::to_owned)
}

fn parse_title(data: &Value) -> Option<Manga> {
    let id = string_field(data, "id").or_else(|| string_field(data, "_id"))?;
    let name = string_field(data, "name").or_else(|| string_field(data, "title"))?;
    let slug = string_field(data, "slug").unwrap_or_else(|| slugify(&name));
    let url = string_field(data, "url")
        .and_then(|url| join_url(BASE_URL, &url).ok())
        .unwrap_or_else(|| format!("{BASE_URL}/title-detail/{slug}-{id}"));
    let mut manga = Manga::new(url, name);
    manga.thumbnail_url = cover_url(data).or_else(|| string_field(data, "img"));
    manga.alt_titles = alternate_names(data);
    manga.demographic = demographic(string_field(data, "publicationDemographic"));
    manga.year = year_of(string_field(data, "date_published"));
    Some(manga)
}

fn parse_chapters(row: &Value) -> Vec<Chapter> {
    let number = row
        .get("number")
        .or_else(|| row.get("chapter_number"))
        .and_then(number_value);
    let title = string_field(row, "title").or_else(|| string_field(row, "name"));
    let Some(translations) = row.get("translations").and_then(Value::as_array) else {
        return parse_chapter(row, number, title).into_iter().collect();
    };

    translations
        .iter()
        .filter_map(|translation| {
            let language =
                string_field(translation, "language").or_else(|| string_field(translation, "lang"));
            if language.as_deref().is_some_and(|language| language != "en") {
                return None;
            }
            parse_chapter(translation, number, title.clone())
        })
        .collect()
}

fn parse_chapter(row: &Value, number: Option<f32>, title: Option<String>) -> Option<Chapter> {
    let language = string_field(row, "language").or_else(|| string_field(row, "lang"));
    if language.as_deref().is_some_and(|language| language != "en") {
        return None;
    }
    let id = string_field(row, "id").or_else(|| string_field(row, "id_chapter"))?;
    let name = string_field(row, "name")
        .or(title)
        .unwrap_or_else(|| match number {
            Some(number) => format!("Chapter {number}"),
            None => "Chapter".to_owned(),
        });
    let scanlator = row
        .get("group")
        .and_then(|group| string_field(group, "name"))
        .or_else(|| string_field(row, "group_name"));
    Some(Chapter {
        url: format!("{BASE_URL}/chapter-detail/{id}"),
        name,
        chapter_number: number,
        scanlator,
        uploaded_at: string_field(row, "created_at")
            .or_else(|| string_field(row, "updated_at"))
            .map(|stamp| parse_timestamp(&stamp))
            .unwrap_or(0),
    })
}

fn number_value(value: &Value) -> Option<f32> {
    value
        .as_f64()
        .map(|number| number as f32)
        .or_else(|| value.as_str()?.parse().ok())
}

fn title_id(url: &str) -> Option<String> {
    id_after(url, "/title-detail/")
}

fn title_key(url: &str) -> Option<String> {
    url.split_once("/title-detail/")?
        .1
        .split(['?', '#'])
        .next()?
        .trim_matches('/')
        .split('/')
        .next()
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

fn chapter_id(url: &str) -> Option<String> {
    id_after(url, "/chapter-detail/")
}

fn id_after(url: &str, prefix: &str) -> Option<String> {
    let rest = url.split_once(prefix)?.1;
    rest.split(['?', '#', '/'])
        .next()?
        .split('-')
        .find(|part| part.len() == 24 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// The API answers with lowercase names — `shounen` — while the other source
/// sends them capitalized, so the card would read inconsistently.
fn demographic(value: Option<String>) -> Option<String> {
    let value = value?;
    let mut chars = value.chars();
    let first = chars.next()?;
    let rest = chars.as_str().to_lowercase();
    Some(first.to_uppercase().collect::<String>() + &rest)
}

fn cover_url(data: &Value) -> Option<String> {
    let id = string_field(data, "id")?;
    let filename = data.pointer("/image/cover/name").and_then(Value::as_str)?;
    Some(format!("{COVER_URL}/{id}/{filename}"))
}

fn string_field(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn text_field(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let text = value.as_str().map(str::to_owned).or_else(|| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
    })?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn people_field(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let people = value.as_array()?;
    let names = people
        .iter()
        .filter_map(|person| {
            person
                .as_str()
                .map(str::to_owned)
                .or_else(|| string_field(person, "name"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    (!names.is_empty()).then_some(names)
}

/// Alternate names, deduplicated case-insensitively.
fn alternate_names(data: &Value) -> Vec<String> {
    let mut names: Vec<String> = match data.get("alternateName") {
        Some(Value::Array(rows)) => rows
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(name)) => vec![name.clone()],
        _ => Vec::new(),
    };
    names.retain(|name| !name.trim().is_empty());
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    names
}

fn additional_tags(data: &Value) -> Vec<String> {
    data.get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tag| {
            !matches!(
                tag.get("group").and_then(Value::as_str),
                Some("genre" | "demographic")
            )
        })
        .filter_map(|tag| string_field(tag, "name"))
        .collect()
}

/// Tags are grouped; the demographic sometimes lives there instead.
fn tag_group(data: &Value, group: &str) -> Option<String> {
    let tags = data.get("tags")?.as_array()?;
    let names = tags
        .iter()
        .filter(|tag| tag.get("group").and_then(Value::as_str) == Some(group))
        .filter_map(|tag| string_field(tag, "name"))
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join(", "))
}

/// The first four-digit year in a date string.
fn year_of(text: Option<String>) -> Option<u16> {
    text?
        .split(|c: char| !c.is_ascii_digit())
        .find(|part| part.len() == 4)
        .and_then(|part| part.parse::<u16>().ok())
        .filter(|year| (1900..=2100).contains(year))
}

fn slugify(title: &str) -> String {
    title
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_status(status: &str) -> MangaStatus {
    match status.to_ascii_lowercase().as_str() {
        "ongoing" => MangaStatus::Ongoing,
        "completed" | "finished" => MangaStatus::Completed,
        "licensed" => MangaStatus::Licensed,
        "cancelled" | "canceled" => MangaStatus::Cancelled,
        "hiatus" | "on hiatus" => MangaStatus::OnHiatus,
        _ => MangaStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_source_tags_beyond_genres_and_demographics() {
        let data = serde_json::json!({"tags": [
            {"name": "Adventure", "group": "genre"},
            {"name": "Shounen", "group": "demographic"},
            {"name": "School Life", "group": "theme"},
            {"name": "Full Color"},
            {"group": "theme"}
        ]});
        assert_eq!(additional_tags(&data), ["School Life", "Full Color"]);
    }

    #[test]
    fn parses_title_and_cover() {
        let title = parse_title(&serde_json::json!({
            "id": "68516a86702284f83417c943",
            "name": "Childhood Friend Complex",
            "slug": "childhood-friend-complex",
            "image": {"cover": {"name": "cover.webp"}}
        }))
        .unwrap();
        assert_eq!(title.title, "Childhood Friend Complex");
        assert_eq!(
            title.url,
            "https://mangaball.com/title-detail/childhood-friend-complex-68516a86702284f83417c943"
        );
        assert_eq!(
            title.thumbnail_url.as_deref(),
            Some("https://bulbasaur.poke-black-and-white.net/covers/68516a86702284f83417c943/cover.webp")
        );
    }

    #[test]
    fn keeps_english_chapters_and_reads_page_lists() {
        let chapters = parse_chapters(&serde_json::json!({
            "number": 95,
            "title": "Chapter 95",
            "translations": [
                {"id_chapter": "6b013f1f26a731bc7a929a2f", "language": "en",
                 "name": "Chapter 95", "group": {"name": "Seadra"}},
                {"id_chapter": "6b013f1f26a731bc7a929a30", "language": "es"}
            ]
        }));
        assert_eq!(chapters.len(), 1);
        let chapter = &chapters[0];
        assert_eq!(chapter.chapter_number, Some(95.0));
        assert_eq!(chapter.scanlator.as_deref(), Some("Seadra"));
        assert_eq!(
            chapter_id(&chapter.url).as_deref(),
            Some("6b013f1f26a731bc7a929a2f")
        );
        assert!(parse_chapters(&serde_json::json!({"id":"abc", "lang":"es"})).is_empty());
        assert_eq!(
            title_id("https://mangaball.com/title-detail/childhood-friend-complex-68516a86702284f83417c943/"),
            Some("68516a86702284f83417c943".to_owned())
        );
    }

    #[test]
    fn reads_both_pagination_shapes() {
        let response = serde_json::json!({
            "pagination": {"current_page": 2, "last_page": 4}
        });
        assert_eq!(total_pages(&response, 2), 4);

        let response = serde_json::json!({
            "pagination": {"page": 2, "total_pages": 4}
        });
        assert_eq!(total_pages(&response, 2), 4);
    }

    #[test]
    fn reads_image_urls_from_strings_and_objects() {
        assert_eq!(
            page_url(&serde_json::json!("https://cdn.example/page.webp")),
            Some("https://cdn.example/page.webp".to_owned())
        );
        assert_eq!(
            page_url(&serde_json::json!({"image_url": "https://cdn.example/page.webp"})),
            Some("https://cdn.example/page.webp".to_owned())
        );
    }
}
