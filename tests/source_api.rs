//! Exercises the extension API the way a real extension would use it.

use rustga::filters::{Filter, FilterValue, FilterValues, TriState};
use rustga::models::{Chapter, Manga, MangaPage, Page};
use rustga::source::{derive_id, Source, SourceInfo};
use rustga::{Error, Registry, Result};

struct MockSource {
    info: SourceInfo,
}

impl MockSource {
    fn new() -> Self {
        Self {
            info: SourceInfo::new("Mock", "EN", 1, "https://example.com/"),
        }
    }

    fn series(&self) -> Manga {
        let mut manga = Manga::new("/series/1", "Test Series");
        manga.author = Some("Nobody".to_owned());
        manga
    }
}

impl Source for MockSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn filters(&self) -> Vec<Filter> {
        vec![Filter::TriState {
            name: "Status".to_owned(),
        }]
    }

    fn popular(&self, page: i64) -> Result<MangaPage> {
        Ok(MangaPage::new(vec![self.series()], page < 2))
    }

    fn search(&self, query: &str, _page: i64, filters: &FilterValues) -> Result<MangaPage> {
        let mut manga = self.series();
        manga.title = format!("{query}/{:?}", filters.tri_state("Status"));
        Ok(MangaPage::single(vec![manga]))
    }

    fn details(&self, manga: &Manga) -> Result<Manga> {
        let mut detailed = manga.clone();
        detailed.description = Some("Synopsis".to_owned());
        Ok(detailed)
    }

    fn chapters(&self, _manga: &Manga) -> Result<Vec<Chapter>> {
        Ok(vec![Chapter {
            url: "/chapter/1".to_owned(),
            name: "Chapter 1".to_owned(),
            chapter_number: Some(1.0),
            scanlator: None,
            uploaded_at: 1_700_000_000,
        }])
    }

    fn pages(&self, _chapter: &Chapter) -> Result<Vec<Page>> {
        Ok(vec![Page {
            index: 0,
            url: "https://example.com/1.jpg".to_owned(),
            image_url: None,
        }])
    }
}

#[test]
fn info_normalises_language_and_base_url() {
    let source = MockSource::new();
    assert_eq!(source.info().lang, "en");
    assert_eq!(source.info().base_url, "https://example.com");
    assert_eq!(source.info().id, derive_id("Mock", "en", 1));
}

#[test]
fn ids_change_with_versions_and_languages() {
    let base = derive_id("Dex", "en", 1);
    assert_eq!(base, derive_id("Dex", "en", 1));
    assert_ne!(base, derive_id("Dex", "en", 2));
    assert_ne!(base, derive_id("Dex", "ja", 1));
    assert!(base < 1 << 63, "id must fit a signed 64-bit range");
}

#[test]
fn registry_finds_sources_by_id() {
    let registry = Registry::new(vec![Box::new(MockSource::new())]);
    let id = registry.all()[0].info().id;
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry.by_id(id).map(|source| source.info().name.as_str()),
        Some("Mock")
    );
    assert!(registry.by_id(id + 1).is_none());
}

#[test]
fn a_source_round_trips_through_the_trait_object() {
    let source: Box<dyn Source> = Box::new(MockSource::new());
    let listing = source.popular(1).expect("popular should succeed");
    assert!(listing.has_next_page);

    let manga = source.details(&listing.mangas[0]).expect("details");
    assert_eq!(manga.author.as_deref(), Some("Nobody"));
    assert_eq!(manga.description.as_deref(), Some("Synopsis"));

    let chapters = source.chapters(&manga).expect("chapters");
    let pages = source.pages(&chapters[0]).expect("pages");
    assert_eq!(
        source.image_url(&pages[0]).unwrap(),
        "https://example.com/1.jpg"
    );
}

#[test]
fn filters_carry_into_search() {
    let source = MockSource::new();
    let mut values = FilterValues::new();
    values.set("Status", FilterValue::TriState(TriState::Exclude));
    assert_eq!(values.tri_state("Status"), TriState::Exclude);
    assert_eq!(values.text("Missing"), None);

    let results = source.search("duck", 1, &values).expect("search");
    assert_eq!(results.mangas[0].title, "duck/Exclude");
}

#[test]
fn unimplemented_capabilities_report_unsupported() {
    let source = MockSource::new();
    assert!(!source.supports_latest());
    assert!(matches!(source.latest(1), Err(Error::Unsupported(_))));
}
