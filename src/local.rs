//! Local chapter cache and reader preferences. Never stores image bytes or credentials.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::{models::Chapter, source::ChapterList};

/// Finder launches apps without a writable working directory.
pub fn directory() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        home.map(|p| p.join("Library/Application Support/Rustga"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Rustga"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|p| p.join(".local/share")))
            .map(|p| p.join("rustga"))
    }
    .unwrap_or_else(|| PathBuf::from(".rustga"))
}

pub fn chapter_path(root: &Path, source: u64, series: &str) -> PathBuf {
    // Keep URL delimiters out of filenames and normalize MangaDot's relative URLs.
    let series = series.trim_end_matches('/');
    let key = series.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    root.join(format!("chapters-{source}-{key:016x}.json"))
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    fs::create_dir_all(path.parent().expect("local file has a parent"))?;
    fs::write(&temporary, serde_json::to_vec(value)?)?;
    fs::rename(temporary, path)
}

pub fn number_key(chapter: &Chapter) -> Option<u32> {
    chapter
        .chapter_number
        .filter(|n| n.is_finite())
        .map(|n| if n == 0.0 { 0 } else { n.to_bits() })
}

pub fn ordered_indices(chapters: &[Chapter], hide_duplicates: bool) -> Vec<usize> {
    let mut order: Vec<_> = (0..chapters.len()).collect();
    order.sort_by(|&a, &b| {
        let a = &chapters[a];
        let b = &chapters[b];
        // `-0.0` and `0.0` are the same chapter; total_cmp ranks them apart.
        let number = |c: &Chapter| {
            c.chapter_number
                .filter(|n| n.is_finite())
                .map(|n| if n == 0.0 { 0.0 } else { n })
                .unwrap_or(f32::NEG_INFINITY)
        };
        number(b)
            .total_cmp(&number(a))
            .then_with(|| b.scanlator.is_some().cmp(&a.scanlator.is_some()))
            .then_with(|| a.url.cmp(&b.url))
    });
    if hide_duplicates {
        let mut seen = std::collections::HashSet::new();
        order.retain(|&i| number_key(&chapters[i]).is_none_or(|n| seen.insert(n)));
    }
    order
}

pub fn save_chapters(path: &Path, list: &ChapterList) -> io::Result<()> {
    // The refresh may have been interrupted. Never discard coverage already on disk.
    let mut list = list.clone();
    if let Some(previous) = read::<ChapterList>(path)? {
        let mut urls: std::collections::HashSet<_> =
            list.chapters.iter().map(|c| c.url.clone()).collect();
        list.chapters.extend(
            previous
                .chapters
                .into_iter()
                .filter(|c| urls.insert(c.url.clone())),
        );
    }
    write(path, &list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_numbered_duplicates_and_prefers_a_named_release() {
        let chapters = [
            (Some(0.0), None),
            (Some(-0.0), Some("Group")),
            (Some(0.5), None),
            (None, None),
            (None, None),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (chapter_number, group))| Chapter {
            url: i.to_string(),
            chapter_number,
            scanlator: group.map(str::to_owned),
            ..Default::default()
        })
        .collect::<Vec<_>>();
        assert_eq!(ordered_indices(&chapters, true), [2, 1, 3, 4]);
        assert_eq!(ordered_indices(&chapters, false).len(), 5);
    }

    #[test]
    fn incomplete_refresh_preserves_disk_coverage_and_is_labeled_partial() {
        let directory = PathBuf::from(format!("target/cache-test-{}", std::process::id()));
        let path = chapter_path(&directory, 7, "/manga/1");
        let chapter = |url: &str| Chapter {
            url: url.into(),
            ..Default::default()
        };
        save_chapters(
            &path,
            &ChapterList {
                chapters: vec![chapter("old")],
                complete: true,
            },
        )
        .unwrap();
        save_chapters(
            &path,
            &ChapterList {
                chapters: vec![chapter("new")],
                complete: false,
            },
        )
        .unwrap();
        let reopened = read::<ChapterList>(&path).unwrap().unwrap();
        assert!(!reopened.complete);
        assert_eq!(reopened.chapters, [chapter("new"), chapter("old")]);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
