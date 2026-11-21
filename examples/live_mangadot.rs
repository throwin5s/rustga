//! Live smoke test for the MangaDot extension.
//!
//! Hits the real site, so it is not part of `cargo test`:
//!
//! ```sh
//! cargo run --example live_mangadot
//! ```

use rustga::filters::FilterValues;
use rustga::models::Manga;
use rustga::source::ChapterList;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = rustga::extensions::builtin()
        .into_iter()
        .find(|source| source.info().name == "MangaDot")
        .ok_or("MangaDot not compiled in")?;

    println!(
        "source: {} ({})",
        source.info().name,
        source.info().base_url
    );

    let popular = source.popular(1)?;
    println!(
        "popular: {} items, next page: {}, first: {}",
        popular.mangas.len(),
        popular.has_next_page,
        popular.mangas[0].title
    );

    let latest = source.latest(1)?;
    println!(
        "latest: {} items, first: {}",
        latest.mangas.len(),
        latest.mangas[0].title
    );

    let found = source.search("rezero", 1, &FilterValues::new())?;
    println!(
        "search: {} items, next page: {}, first: {}",
        found.mangas.len(),
        found.has_next_page,
        found.mangas[0].title
    );

    let detailed = source.details(&found.mangas[0])?;
    println!(
        "details: {} | {} | {} | {}",
        detailed.title,
        detailed.author.as_deref().unwrap_or("-"),
        detailed.genre.as_deref().unwrap_or("-"),
        detailed.status as u8
    );

    let chapters = source.chapters(&detailed)?;
    println!("chapters: {}", chapters.len());
    if let Some(chapter) = chapters.first() {
        println!(
            "  newest: {} (number {:?})",
            chapter.name, chapter.chapter_number
        );
    }

    if let Some(chapter) = chapters
        .iter()
        .find(|chapter| chapter.chapter_number == Some(1.0))
    {
        let pages = source.pages(chapter)?;
        println!("pages: {} first: {}", pages.len(), pages[0].url);

        // Image bytes, including whatever headers the site expects.
        let http = rustga::HttpSource::new(0)?;
        let bytes = http.get_bytes(&pages[0].url)?;
        println!("first image: {} bytes", bytes.len());
    }

    // Omniscient Reader: 312 chapters, which the SSR page does not ship. Measure
    // the first usable list separately from full coverage, then reopen from disk.
    let long = Manga::new("/manga/175", "Omniscient Reader");
    let directory = std::path::Path::new("target/live-cache");
    let path = rustga::local::chapter_path(
        directory,
        source.info().id,
        "https://mangadot.net/manga/175",
    );
    let _ = std::fs::remove_file(&path);

    let started = std::time::Instant::now();
    let mut first = None;
    let mut latest = ChapterList::default();
    source.refresh_chapters(&long, ChapterList::default(), &mut |list| {
        if first.is_none() {
            first = Some((started.elapsed(), list.chapters.len(), list.complete));
        }
        latest = list.clone();
        true
    })?;
    let cold_elapsed = started.elapsed();

    let numbers: Vec<f32> = latest
        .chapters
        .iter()
        .filter_map(|c| c.chapter_number)
        .collect();
    let distinct: std::collections::HashSet<u32> =
        numbers.iter().map(|number| *number as u32).collect();
    let missing: Vec<u32> = (0..=311u32)
        .filter(|n| !numbers.contains(&(*n as f32)))
        .collect();
    let visible = rustga::local::ordered_indices(&latest.chapters, true);
    println!(
        "Omniscient Reader cold: first usable list {:?} ({} rows), complete in {:?}",
        first.map(|f| f.0),
        first.map(|f| f.1).unwrap_or(0),
        cold_elapsed
    );
    println!(
        "  {} rows / {} distinct chapters, {} visible with duplicates hidden, missing {}",
        latest.chapters.len(),
        distinct.len(),
        visible.len(),
        missing.len()
    );

    rustga::local::save_chapters(&path, &latest)?;
    let reopened = rustga::local::read::<ChapterList>(&path)?.unwrap_or_default();
    let started = std::time::Instant::now();
    let mut reopen_first = None;
    source.refresh_chapters(&long, reopened, &mut |list| {
        if reopen_first.is_none() {
            reopen_first = Some((started.elapsed(), list.chapters.len()));
        }
        true
    })?;
    println!(
        "  reopen from disk: first list {:?}, refresh done in {:?}",
        reopen_first.map(|f| f.0),
        started.elapsed()
    );

    Ok(())
}
