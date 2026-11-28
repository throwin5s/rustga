//! C ABI over the rustga core, for the SwiftUI host.
//!
//! Structured data crosses as JSON, which keeps the Swift side on `Codable` and
//! needs no generated bindings. Page images do not fit that shape — they are not
//! text and can contain NUL — so they cross as a length-carrying
//! [`RustgaBuffer`] instead.
//!
//! Calls block on the network; the host must run them off the main thread.

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rustga::models::{Chapter, Manga, Page};
use rustga::{filters::FilterValues, Registry};
use serde::de::DeserializeOwned;
use serde::Serialize;

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::builtin)
}

fn source_of(id: u64) -> Result<&'static dyn rustga::Source, String> {
    registry()
        .by_id(id)
        .ok_or_else(|| format!("unknown source {id}"))
}

fn parse<T: DeserializeOwned>(json: &str) -> Result<T, String> {
    serde_json::from_str(json).map_err(|error| format!("bad request: {error}"))
}

#[derive(Serialize)]
struct SourceRow {
    id: u64,
    name: String,
    lang: String,
    browse_only: bool,
}

#[derive(Serialize)]
struct Reply<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn envelope<T: Serialize>(result: Result<T, String>) -> String {
    let reply = match result {
        Ok(data) => Reply {
            ok: true,
            data: Some(data),
            error: None,
        },
        Err(error) => Reply::<T> {
            ok: false,
            data: None,
            error: Some(error),
        },
    };
    serde_json::to_string(&reply).unwrap_or_else(|_| panic!("a reply is always encodable"))
}

fn into_c_string(text: String) -> *mut c_char {
    CString::new(text)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

fn reply_of<T: Serialize>(result: Result<T, String>) -> String {
    catch_unwind(AssertUnwindSafe(|| envelope(result)))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"panic\"}".to_owned())
}

fn sources() -> Vec<SourceRow> {
    registry()
        .all()
        .iter()
        .map(|source| {
            let info = source.info();
            SourceRow {
                id: info.id,
                name: info.name.clone(),
                lang: info.lang.clone(),
                browse_only: source.browse_only(),
            }
        })
        .collect()
}

fn popular(source_id: u64, page: i64) -> Result<Vec<Manga>, String> {
    source_of(source_id)?
        .popular(page)
        .map(|listing| listing.mangas)
        .map_err(|error| error.to_string())
}

fn sections(source_id: u64) -> Result<Vec<rustga::models::MangaSection>, String> {
    source_of(source_id)?
        .sections()
        .map_err(|error| error.to_string())
}

fn search(source_id: u64, query: &str, page: i64) -> Result<Vec<Manga>, String> {
    source_of(source_id)?
        .search(query, page, &FilterValues::new())
        .map(|listing| listing.mangas)
        .map_err(|error| error.to_string())
}

fn details(source_id: u64, manga_json: &str) -> Result<Manga, String> {
    let manga: Manga = parse(manga_json)?;
    source_of(source_id)?
        .details(&manga)
        .map_err(|error| error.to_string())
}

fn chapters(source_id: u64, manga_json: &str) -> Result<Vec<Chapter>, String> {
    let manga: Manga = parse(manga_json)?;
    source_of(source_id)?
        .chapters(&manga)
        .map_err(|error| error.to_string())
}

fn pages(source_id: u64, chapter_json: &str) -> Result<Vec<Page>, String> {
    pages_at(&rustga::local::directory(), source_id, chapter_json)
}

fn pages_at(root: &Path, source_id: u64, chapter_json: &str) -> Result<Vec<Page>, String> {
    let chapter: Chapter = parse(chapter_json)?;
    // A completed download is authoritative: it needs no network and its page
    // list is exactly what the saved image files are keyed to.
    if let Some(pages) = rustga::downloads::pages(root, source_id, &chapter.url)
        .map_err(|error| error.to_string())?
    {
        return Ok(pages);
    }
    source_of(source_id)?
        .pages(&chapter)
        .map_err(|error| error.to_string())
}

fn image(source_id: u64, page_json: &str) -> Result<Vec<u8>, String> {
    image_at(&rustga::local::directory(), source_id, page_json)
}

fn image_at(root: &Path, source_id: u64, page_json: &str) -> Result<Vec<u8>, String> {
    let page: Page = parse(page_json)?;
    let url = page.image_url.as_deref().unwrap_or(&page.url);
    if let Some(bytes) =
        rustga::downloads::read_page(root, url).map_err(|error| error.to_string())?
    {
        return Ok(bytes);
    }
    // The source fetches its own images: its headers and clearance cookie are
    // part of the request (MangaBall's CDN requires its Referer).
    source_of(source_id)?
        .image(&page)
        .map_err(|error| error.to_string())
}

/// One download running on a worker thread. The host starts it, polls it and
/// may cancel it; nothing here blocks the caller.
struct DownloadJob {
    cancel: Arc<AtomicBool>,
    state: Mutex<JobState>,
}

struct JobState {
    running: bool,
    message: String,
    error: Option<String>,
}

impl DownloadJob {
    fn report(&self, message: String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.message = message;
    }

    fn finish(&self, result: Result<(), String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running = false;
        match result {
            Ok(()) => state.message = "Download complete".to_owned(),
            Err(error) => state.error = Some(error),
        }
    }
}

/// What a poll gets back: the last progress line, and whether it still runs.
#[derive(Serialize)]
struct DownloadSnapshot {
    running: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

static JOBS: OnceLock<Mutex<HashMap<u64, Arc<DownloadJob>>>> = OnceLock::new();
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn jobs() -> &'static Mutex<HashMap<u64, Arc<DownloadJob>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn job_of(id: u64) -> Result<Arc<DownloadJob>, String> {
    jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("unknown download {id}"))
}

/// Downloads land in the same application-data directory the desktop host uses,
/// so both hosts see one library.
fn downloads(source_id: u64) -> Result<Vec<rustga::downloads::DownloadedManga>, String> {
    rustga::downloads::library(&rustga::local::directory(), source_id)
        .map_err(|error| error.to_string())
}

fn start_download(source_id: u64, manga_json: &str, chapters_json: &str) -> Result<u64, String> {
    let manga: Manga = parse(manga_json)?;
    let chapters: Vec<Chapter> = parse(chapters_json)?;
    if chapters.is_empty() {
        return Err("no chapters to download".to_owned());
    }
    let source = source_of(source_id)?;
    let root = rustga::local::directory();
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let job = Arc::new(DownloadJob {
        cancel: Arc::new(AtomicBool::new(false)),
        state: Mutex::new(JobState {
            running: true,
            message: "Starting download…".to_owned(),
            error: None,
        }),
    });
    jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, job.clone());
    let worker = job.clone();
    std::thread::spawn(move || {
        let result = rustga::downloads::download(
            &root,
            source,
            &manga,
            &chapters,
            worker.cancel.as_ref(),
            |_, message| worker.report(message),
        );
        worker.finish(result);
    });
    Ok(id)
}

fn download_snapshot(id: u64) -> Result<DownloadSnapshot, String> {
    let job = job_of(id)?;
    let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
    Ok(DownloadSnapshot {
        running: state.running,
        message: state.message.clone(),
        error: state.error.clone(),
    })
}

fn cancel_download(id: u64) {
    if let Ok(job) = job_of(id) {
        job.cancel.store(true, Ordering::Relaxed);
        job.report("Cancelling…".to_owned());
    }
}

fn release_download(id: u64) {
    if let Ok(job) = job_of(id) {
        job.cancel.store(true, Ordering::Relaxed);
    }
    jobs().lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

/// A foreign-owned byte buffer. `error` is set instead of `data` on failure.
///
/// Free it with [`rustga_buffer_free`].
#[repr(C)]
pub struct RustgaBuffer {
    pub data: *mut u8,
    pub len: usize,
    pub error: *mut c_char,
}

/// JSON array of `{ id, name, lang }`. Caller frees the result.
#[no_mangle]
pub extern "C" fn rustga_sources_json() -> *mut c_char {
    into_c_string(reply_of(Ok(sources())))
}

/// Extract recommendations from a pasted thread. AI settings are optional JSON.
///
/// # Safety
/// `thread` and `settings` must be valid null-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn rustga_discover_json(
    thread: *const c_char,
    settings: *const c_char,
) -> *mut c_char {
    let Some(thread) = string_arg(thread) else {
        return into_c_string(envelope::<Vec<rustga::discover::Recommendation>>(Err(
            "missing thread".into(),
        )));
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut found = rustga::discover::heuristic(&thread);
        if let Some(json) = string_arg(settings) {
            let ai: rustga::discover::AiSettings = parse(&json)?;
            let url = reqwest_url_https(&ai.base_url)?;
            let _ = url;
            for rec in rustga::discover::chat(&ai, &thread)? {
                if !found.iter().any(|existing| {
                    rustga::discover::normalize_title(&existing.title)
                        == rustga::discover::normalize_title(&rec.title)
                }) {
                    found.push(rec);
                }
            }
        }
        Ok(found)
    }))
    .unwrap_or_else(|_| Err("panic".into()));
    into_c_string(envelope(result))
}

fn reqwest_url_https(base: &str) -> Result<(), String> {
    if !base.starts_with("https://") || base[8..].is_empty() {
        return Err("AI endpoint must use HTTPS".into());
    }
    Ok(())
}

/// Named homepage shelves for a source, if it publishes any.
#[no_mangle]
pub extern "C" fn rustga_sections_json(source_id: u64) -> *mut c_char {
    into_c_string(reply_of(sections(source_id)))
}

/// Popular listing for a source, same envelope as [`rustga_search_json`].
#[no_mangle]
pub extern "C" fn rustga_popular_json(source_id: u64, page: i64) -> *mut c_char {
    into_c_string(reply_of(popular(source_id, page)))
}

/// JSON `{ ok, data | error }` where `data` is an array of manga.
///
/// # Safety
///
/// `query` must be a valid null-terminated C string. Caller frees the result.
#[no_mangle]
pub unsafe extern "C" fn rustga_search_json(
    source_id: u64,
    query: *const c_char,
    page: i64,
) -> *mut c_char {
    if query.is_null() {
        return into_c_string(envelope::<Vec<Manga>>(Err("missing query".to_owned())));
    }
    let query = CStr::from_ptr(query).to_string_lossy().into_owned();
    into_c_string(reply_of(search(source_id, &query, page)))
}

/// Full series details. `manga_json` is a `Manga` as returned by a listing.
///
/// # Safety
///
/// `manga_json` must be a valid null-terminated C string. Caller frees the result.
#[no_mangle]
pub unsafe extern "C" fn rustga_details_json(
    source_id: u64,
    manga_json: *const c_char,
) -> *mut c_char {
    let Some(json) = string_arg(manga_json) else {
        return into_c_string(envelope::<Manga>(Err("missing manga".to_owned())));
    };
    into_c_string(reply_of(details(source_id, &json)))
}

/// Chapter list for a series.
///
/// # Safety
///
/// `manga_json` must be a valid null-terminated C string. Caller frees the result.
#[no_mangle]
pub unsafe extern "C" fn rustga_chapters_json(
    source_id: u64,
    manga_json: *const c_char,
) -> *mut c_char {
    let Some(json) = string_arg(manga_json) else {
        return into_c_string(envelope::<Vec<Chapter>>(Err("missing manga".to_owned())));
    };
    into_c_string(reply_of(chapters(source_id, &json)))
}

/// Page list for one chapter. Uses the saved download when the chapter is
/// downloaded, so a saved chapter can be read without the source.
///
/// # Safety
///
/// `chapter_json` must be a valid null-terminated C string. Caller frees the result.
#[no_mangle]
pub unsafe extern "C" fn rustga_pages_json(
    source_id: u64,
    chapter_json: *const c_char,
) -> *mut c_char {
    let Some(json) = string_arg(chapter_json) else {
        return into_c_string(envelope::<Vec<Page>>(Err("missing chapter".to_owned())));
    };
    into_c_string(reply_of(pages(source_id, &json)))
}

/// One page's image bytes, read from the download when it is saved. Uses the
/// buffer shape because images are not text.
///
/// # Safety
///
/// `page_json` must be a valid null-terminated C string. Free the result with
/// [`rustga_buffer_free`].
#[no_mangle]
pub unsafe extern "C" fn rustga_image(source_id: u64, page_json: *const c_char) -> RustgaBuffer {
    let result = match string_arg(page_json) {
        Some(json) => catch_unwind(AssertUnwindSafe(|| image(source_id, &json)))
            .unwrap_or_else(|_| Err("panic".to_owned())),
        None => Err("missing page".to_owned()),
    };
    match result {
        // A boxed slice guarantees capacity == len, so freeing with `len` is sound.
        Ok(bytes) => {
            let boxed: Box<[u8]> = bytes.into_boxed_slice();
            let len = boxed.len();
            RustgaBuffer {
                data: Box::into_raw(boxed).cast::<u8>(),
                len,
                error: std::ptr::null_mut(),
            }
        }
        Err(error) => RustgaBuffer {
            data: std::ptr::null_mut(),
            len: 0,
            error: into_c_string(error),
        },
    }
}

/// Save chapters for offline reading on a worker thread. `data` is the job id
/// to pass to [`rustga_download_status_json`] and friends.
///
/// # Safety
///
/// `manga_json` and `chapters_json` must be valid null-terminated C strings.
/// Caller frees the result.
#[no_mangle]
pub unsafe extern "C" fn rustga_download_start_json(
    source_id: u64,
    manga_json: *const c_char,
    chapters_json: *const c_char,
) -> *mut c_char {
    let (Some(manga), Some(chapters)) = (string_arg(manga_json), string_arg(chapters_json)) else {
        return into_c_string(envelope::<u64>(Err("missing manga or chapters".to_owned())));
    };
    into_c_string(reply_of(start_download(source_id, &manga, &chapters)))
}

/// Progress of a download: `{ running, message, error? }`.
///
/// # Safety
///
/// The caller must free the result.
#[no_mangle]
pub extern "C" fn rustga_download_status_json(job: u64) -> *mut c_char {
    into_c_string(reply_of(download_snapshot(job)))
}

/// Ask a download to stop at the next page boundary. The status then reports
/// the cancellation as an error.
#[no_mangle]
pub extern "C" fn rustga_download_cancel(job: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| cancel_download(job)));
}

/// Forget a finished job. Safe to call while it still runs: it is cancelled and
/// the worker finishes on its own.
#[no_mangle]
pub extern "C" fn rustga_download_release(job: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| release_download(job)));
}

/// JSON array of `{ manga, chapters }` already saved for a source.
///
/// # Safety
///
/// The caller must free the result.
#[no_mangle]
pub extern "C" fn rustga_downloads_json(source_id: u64) -> *mut c_char {
    into_c_string(reply_of(downloads(source_id)))
}

/// Release a buffer returned by [`rustga_image`].
///
/// # Safety
///
/// The buffer must come from this library and must not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn rustga_buffer_free(buffer: RustgaBuffer) {
    if !buffer.data.is_null() {
        let slice = std::ptr::slice_from_raw_parts_mut(buffer.data, buffer.len);
        drop(Box::from_raw(slice));
    }
    if !buffer.error.is_null() {
        drop(CString::from_raw(buffer.error));
    }
}

/// Release a string returned by this library.
///
/// # Safety
///
/// `pointer` must come from this library and must not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn rustga_string_free(pointer: *mut c_char) {
    if !pointer.is_null() {
        drop(CString::from_raw(pointer));
    }
}

/// # Safety
///
/// `pointer` must be null or a valid null-terminated C string.
unsafe fn string_arg(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustga::models::MangaPage;
    use rustga::source::SourceInfo;
    use std::sync::atomic::AtomicUsize;

    /// Serves one chapter and counts what it is asked for, so a read that never
    /// reaches the source is visible.
    struct SavedSource {
        info: SourceInfo,
        pages_calls: AtomicUsize,
        image_calls: AtomicUsize,
        image_bytes: Vec<u8>,
    }

    impl rustga::Source for SavedSource {
        fn info(&self) -> &SourceInfo {
            &self.info
        }
        fn popular(&self, _: i64) -> rustga::Result<MangaPage> {
            unreachable!()
        }
        fn details(&self, _: &Manga) -> rustga::Result<Manga> {
            unreachable!()
        }
        fn chapters(&self, _: &Manga) -> rustga::Result<Vec<Chapter>> {
            unreachable!()
        }
        fn pages(&self, _: &Chapter) -> rustga::Result<Vec<Page>> {
            self.pages_calls.fetch_add(1, Ordering::Relaxed);
            Ok((0..2)
                .map(|index| Page {
                    index,
                    url: format!("https://example.com/{index}.png"),
                    image_url: None,
                })
                .collect())
        }
        fn image(&self, _: &Page) -> rustga::Result<Vec<u8>> {
            self.image_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.image_bytes.clone())
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn a_saved_chapter_reads_from_downloads_without_the_source() {
        let root = std::env::temp_dir().join(format!("rustga-ffi-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = SavedSource {
            info: SourceInfo::new("Saved", "en", 1, "https://example.com"),
            pages_calls: AtomicUsize::new(0),
            image_calls: AtomicUsize::new(0),
            image_bytes: png(),
        };
        let id = source.info.id;
        assert!(source_of(id).is_err(), "the test source is not registered");
        let manga = Manga::new("https://example.com/manga", "Example");
        let chapter = Chapter {
            url: "https://example.com/chapter".into(),
            ..Default::default()
        };
        rustga::downloads::download(
            &root,
            &source,
            &manga,
            std::slice::from_ref(&chapter),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();

        let chapter_json = serde_json::to_string(&chapter).unwrap();
        let saved = pages_at(&root, id, &chapter_json).unwrap();
        assert_eq!(saved.len(), 2);

        for page in &saved {
            let page_json = serde_json::to_string(page).unwrap();
            assert_eq!(image_at(&root, id, &page_json).unwrap(), source.image_bytes);
        }

        // Only the download fetched anything; reading the saved chapter and
        // every saved page stayed on disk, and this source is not registered,
        // so any fallback would have failed outright.
        assert_eq!(source.pages_calls.load(Ordering::Relaxed), 1);
        assert_eq!(source.image_calls.load(Ordering::Relaxed), 2);

        // A chapter with no download still reaches the source, and this test
        // source is not registered, so that read fails.
        let missing = Chapter {
            url: "https://example.com/missing".into(),
            ..Default::default()
        };
        assert!(pages_at(&root, id, &serde_json::to_string(&missing).unwrap()).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
