//! The extension trait every source implements.
//!
//! Shaped after Mihon's `Source` / `CatalogueSource`: the same calls in the same
//! order, minus the Android specifics. Differences are deliberate:
//!
//! - `getMangaUpdate(manga, chapters, fetchDetails, fetchChapters)` is split into
//!   [`Source::details`] and [`Source::chapters`], which is all a reader needs.
//! - `id` is a stable hash rather than a number baked into the extension.

use crate::error::{Error, Result};
use crate::filters::{Filter, FilterValues};
use crate::models::{Chapter, Manga, MangaPage, Page};

/// Metadata describing one installed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// Stable id derived from `name`, `lang` and `version`; see [`derive_id`].
    /// Library entries reference this, so it must not change casually.
    pub id: u64,
    /// Visible name, e.g. `MangaDex`.
    pub name: String,
    /// ISO 639-1 language code, lowercase.
    pub lang: String,
    /// Bump when a site's URLs change incompatibly; this changes the id.
    pub version: u32,
    /// Site root, without a trailing slash.
    pub base_url: String,
}

impl SourceInfo {
    pub fn new(
        name: impl Into<String>,
        lang: impl Into<String>,
        version: u32,
        base_url: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let lang = lang.into().to_ascii_lowercase();
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let id = derive_id(&name, &lang, version);
        Self {
            id,
            name,
            lang,
            version,
            base_url,
        }
    }
}

/// FNV-1a over `name/lang/version`, with the high bit cleared.
///
/// Stable across runs, machines and releases, unlike the standard library's
/// hashers, and it fits the signed range Mihon uses for source ids. Changing
/// this function orphans every library entry, so treat it as frozen.
pub fn derive_id(name: &str, lang: &str, version: u32) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };

    feed(name.as_bytes());
    feed(b"/");
    feed(lang.as_bytes());
    feed(b"/");
    feed(version.to_string().as_bytes());

    hash & 0x7fff_ffff_ffff_ffff
}

/// A chapter-list snapshot, including whether the source verified its coverage.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChapterList {
    pub chapters: Vec<Chapter>,
    pub complete: bool,
}

/// One website, as a reader needs to see it.
///
/// Implementations are stateless apart from an [`crate::http::HttpSource`], and
/// must be `Send + Sync` because the host calls them from worker threads.
pub trait Source: Send + Sync {
    /// Metadata: name, language, version, base URL and the derived id.
    fn info(&self) -> &SourceInfo;

    /// Whether [`Source::latest`] returns anything meaningful.
    fn supports_latest(&self) -> bool {
        false
    }

    /// Filters offered by this source; the host renders them.
    fn filters(&self) -> Vec<Filter> {
        Vec::new()
    }

    /// Popular / trending listing. `page` starts at 1.
    fn popular(&self, page: i64) -> Result<MangaPage>;

    /// Recently updated listing. `page` starts at 1.
    fn latest(&self, _page: i64) -> Result<MangaPage> {
        Err(Error::Unsupported("latest updates"))
    }

    /// Search. Empty `query` plus empty filters usually means "browse".
    fn search(&self, _query: &str, _page: i64, _filters: &FilterValues) -> Result<MangaPage> {
        Err(Error::Unsupported("search"))
    }

    /// Full details for a series obtained from a listing. Return the input
    /// unchanged for fields the site does not provide.
    fn details(&self, manga: &Manga) -> Result<Manga>;

    /// Every chapter, newest first (the host sorts anyway, but keep it cheap).
    fn chapters(&self, manga: &Manga) -> Result<Vec<Chapter>>;

    /// Publish usable snapshots during a refresh. Returning false cancels the walk.
    fn refresh_chapters(
        &self,
        manga: &Manga,
        _cached: ChapterList,
        publish: &mut dyn FnMut(&ChapterList) -> bool,
    ) -> Result<()> {
        publish(&ChapterList {
            chapters: self.chapters(manga)?,
            complete: true,
        });
        Ok(())
    }

    /// Page list for one chapter, in reading order.
    fn pages(&self, chapter: &Chapter) -> Result<Vec<Page>>;

    /// URL to download for a page. Override when the image URL must be derived
    /// or signed separately from the page list.
    fn image_url(&self, page: &Page) -> Result<String> {
        page.image_url
            .clone()
            .or_else(|| Some(page.url.clone()).filter(|url| !url.is_empty()))
            .ok_or_else(|| Error::NotFound("page has no image url".to_owned()))
    }
}
