//! Permanent downloads, separate from the reader's evictable image cache.
use crate::{
    local,
    models::{Chapter, Manga, Page},
    Source,
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct DownloadedManga {
    pub manga: Manga,
    pub chapters: Vec<Chapter>,
}

fn library_path(root: &Path, source: u64, manga: &str) -> PathBuf {
    root.join("downloads").join(format!(
        "library-{source}-{:016x}.json",
        local::url_key(manga)
    ))
}
pub fn page_path(root: &Path, url: &str) -> PathBuf {
    root.join("downloads/pages")
        .join(format!("{:016x}.page", local::url_key(url)))
}
fn manifest_path(root: &Path, source: u64, chapter: &str) -> PathBuf {
    root.join("downloads").join(format!(
        "chapter-{source}-{:016x}.json",
        local::url_key(chapter)
    ))
}
pub fn library(root: &Path, source: u64) -> io::Result<Vec<DownloadedManga>> {
    let entries = match fs::read_dir(root.join("downloads")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let prefix = format!("library-{source}-");
    let mut mangas = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            && path.extension().is_some_and(|e| e == "json")
        {
            if let Some(manga) = local::read(&path)? {
                mangas.push(manga);
            }
        }
    }
    mangas.sort_by(|a: &DownloadedManga, b| a.manga.title.cmp(&b.manga.title));
    Ok(mangas)
}
pub fn pages(root: &Path, source: u64, chapter: &str) -> io::Result<Option<Vec<Page>>> {
    let pages = local::read::<Vec<Page>>(&manifest_path(root, source, chapter))?;
    Ok(pages.filter(|pages| {
        !pages.is_empty()
            && pages
                .iter()
                .all(|p| page_path(root, p.image_url.as_deref().unwrap_or(&p.url)).is_file())
    }))
}
/// Original bytes of a saved page, exactly as the source returned them.
/// Decoding stays with the caller so each host can pick its own size.
pub fn read_page(root: &Path, url: &str) -> io::Result<Option<Vec<u8>>> {
    match fs::read(page_path(root, url)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn save_image(root: &Path, source: &dyn Source, url: &str) -> Result<(), String> {
    let path = page_path(root, url);
    if path.is_file() {
        return Ok(());
    }
    // A cover is just another picture: the source's own client fetches both,
    // with whatever headers or clearance the site requires.
    let page = Page {
        url: url.to_owned(),
        ..Page::default()
    };
    let bytes = source.image(&page).map_err(|e| e.to_string())?;
    prepare_bytes(root, url, &bytes).map_err(|e| format!("Invalid image: {e}"))?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    let result = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map_err(|e| e.to_string())
}

pub fn download(
    root: &Path,
    source: &dyn Source,
    manga: &Manga,
    chapters: &[Chapter],
    cancel: &AtomicBool,
    progress: impl Fn(Option<&str>, String),
) -> Result<(), String> {
    let id = source.info().id;
    let path = library_path(root, id, &manga.url);
    let mut saved: DownloadedManga = local::read(&path)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| DownloadedManga {
            manga: manga.clone(),
            chapters: Vec::new(),
        });
    saved.manga = manga.clone();
    if cancel.load(Ordering::Relaxed) {
        return Err("Download cancelled".into());
    }
    // Cover failure must not prevent chapters being available offline.
    if let Some(url) = &manga.thumbnail_url {
        let _ = save_image(root, source, url);
    }
    for (position, chapter) in chapters.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Download cancelled".into());
        }
        let pages = match pages(root, id, &chapter.url).map_err(|e| e.to_string())? {
            Some(pages) => pages,
            None => source.pages(chapter).map_err(|e| e.to_string())?,
        };
        if pages.is_empty() {
            return Err(format!("{} has no pages", chapter.name));
        }
        for (index, page) in pages.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("Download cancelled".into());
            }
            progress(
                None,
                format!(
                    "Chapter {} / {} · page {} / {}",
                    position + 1,
                    chapters.len(),
                    index + 1,
                    pages.len()
                ),
            );
            save_image(root, source, page.image_url.as_deref().unwrap_or(&page.url))
                .map_err(|e| format!("{} · page {}: {e}", chapter.name, index + 1))?;
        }
        local::write(&manifest_path(root, id, &chapter.url), &pages).map_err(|e| e.to_string())?;
        if !saved.chapters.iter().any(|c| c.url == chapter.url) {
            saved.chapters.push(chapter.clone());
        }
        local::write(&path, &saved).map_err(|e| e.to_string())?;
        progress(
            Some(&chapter.url),
            format!("Downloaded {} / {} chapters", position + 1, chapters.len()),
        );
    }
    Ok(())
}

/// Display-ready pixels avoid decoding compressed originals on every page turn.
/// This is a bounded cache; the permanent original remains the offline authority.
pub struct PreparedPage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}
fn prepared_path(root: &Path, url: &str) -> PathBuf {
    root.join("prepared-pages")
        .join(format!("{:016x}.page", local::url_key(url)))
}
pub fn read_prepared(root: &Path, url: &str) -> io::Result<Option<PreparedPage>> {
    let bytes = match fs::read(prepared_path(root, url)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if bytes.len() < 8 {
        return Ok(None);
    }
    let width = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    let height = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if width == 0
        || height == 0
        || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(bytes.len() - 8)
        || bytes.len() > 16_000_008
    {
        return Ok(None);
    }
    Ok(Some(PreparedPage {
        width,
        height,
        pixels: bytes[8..].to_vec(),
    }))
}
fn prepare_bytes(root: &Path, url: &str, bytes: &[u8]) -> Result<PreparedPage, String> {
    let image = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = if image.width() <= 1600 && image.height() <= 2500 {
        image.into_rgba8()
    } else {
        image.thumbnail(1600, 2500).into_rgba8()
    };
    let (width, height) = rgba.dimensions();
    let pixels = rgba.into_raw();
    let mut prepared = Vec::with_capacity(pixels.len() + 8);
    prepared.extend_from_slice(&width.to_le_bytes());
    prepared.extend_from_slice(&height.to_le_bytes());
    prepared.extend_from_slice(&pixels);
    // Failure to write the disposable cache must not discard usable pixels.
    let _ = local::cache_page(&prepared_path(root, url), &prepared);
    Ok(PreparedPage {
        width: width as usize,
        height: height as usize,
        pixels,
    })
}
pub fn prepare_page(root: &Path, url: &str) -> Result<PreparedPage, String> {
    if let Some(page) = read_prepared(root, url).map_err(|e| e.to_string())? {
        return Ok(page);
    }
    let bytes = fs::read(page_path(root, url)).map_err(|e| e.to_string())?;
    prepare_bytes(root, url, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::MangaPage, source::SourceInfo};
    struct OfflineSource {
        info: SourceInfo,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Source for OfflineSource {
        fn info(&self) -> &SourceInfo {
            &self.info
        }
        fn popular(&self, _: i64) -> crate::Result<MangaPage> {
            unreachable!()
        }
        fn details(&self, _: &Manga) -> crate::Result<Manga> {
            unreachable!()
        }
        fn chapters(&self, _: &Manga) -> crate::Result<Vec<Chapter>> {
            unreachable!()
        }
        fn pages(&self, _: &Chapter) -> crate::Result<Vec<Page>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok((0..2)
                .map(|index| Page {
                    index,
                    url: format!("https://example.com/{index}.png"),
                    image_url: None,
                })
                .collect())
        }
    }
    #[test]
    fn prepared_pixels_round_trip_and_invalid_cache_is_rebuilt_from_original() {
        let root = PathBuf::from(format!("target/cache-test-prepared-{}", std::process::id()));
        let url = "https://example.com/page.png";
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(7, 9)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let path = page_path(&root, url);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, png.get_ref()).unwrap();
        let expected = prepare_page(&root, url).unwrap();
        let cached = read_prepared(&root, url).unwrap().unwrap();
        assert_eq!((cached.width, cached.height), (7, 9));
        assert_eq!(cached.pixels, expected.pixels);
        fs::write(prepared_path(&root, url), b"broken").unwrap();
        assert!(read_prepared(&root, url).unwrap().is_none());
        assert_eq!(prepare_page(&root, url).unwrap().pixels, expected.pixels);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_download_is_not_published_and_resume_is_available_offline() {
        let root = PathBuf::from(format!(
            "target/cache-test-downloads-{}",
            std::process::id()
        ));
        let source = OfflineSource {
            info: SourceInfo::new("Example", "en", 1, "https://example.com"),
            calls: Default::default(),
        };
        let manga = Manga::new("https://example.com/manga", "Example");
        let chapters = vec![Chapter {
            url: "https://example.com/chapter".into(),
            ..Default::default()
        }];
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        for index in 0..2 {
            let path = page_path(&root, &format!("https://example.com/{index}.png"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, png.get_ref()).unwrap();
        }
        let cancel = AtomicBool::new(false);
        assert!(
            download(&root, &source, &manga, &chapters, &cancel, |_, _| {
                cancel.store(true, Ordering::Relaxed);
            })
            .is_err()
        );
        assert!(pages(&root, source.info.id, &chapters[0].url)
            .unwrap()
            .is_none());
        assert!(library(&root, source.info.id).unwrap().is_empty());
        cancel.store(false, Ordering::Relaxed);
        download(&root, &source, &manga, &chapters, &cancel, |_, _| {}).unwrap();
        let saved = library(&root, source.info.id).unwrap();
        assert_eq!(saved[0].manga, manga);
        assert_eq!(saved[0].chapters, chapters);
        assert_eq!(
            pages(&root, source.info.id, &chapters[0].url)
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            read_page(&root, "https://example.com/0.png")
                .unwrap()
                .unwrap(),
            png.get_ref().clone()
        );
        assert!(library(&root, 0).unwrap().is_empty());
        // A second download uses the completed manifest, with no source call.
        download(&root, &source, &manga, &chapters, &cancel, |_, _| {}).unwrap();
        assert_eq!(source.calls.load(Ordering::Relaxed), 2);
        assert_eq!(library(&root, source.info.id).unwrap()[0].chapters.len(), 1);
        fs::remove_file(page_path(&root, "https://example.com/0.png")).unwrap();
        assert!(pages(&root, source.info.id, &chapters[0].url)
            .unwrap()
            .is_none());
        assert!(read_page(&root, "https://example.com/0.png")
            .unwrap()
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
