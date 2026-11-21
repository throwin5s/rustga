//! rustga — a minimal desktop reader.
//!
//! Cover browsing, series details and a fitted-page / webtoon / book reader.
//! Network requests and image decoding stay off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use eframe::egui;

use rustga::filters::FilterValues;
use rustga::models::{Chapter, Manga, MangaPage, Page};
use rustga::{downloads, local, source::ChapterList, Source};

mod book;
#[cfg(target_os = "macos")]
mod verification;

/// Width every page is scaled to before it becomes a texture; the originals are
/// far larger than any screen and would exhaust GPU memory across a chapter.
const PAGE_WIDTH: u32 = 800;

/// Pages kept ready on either side of the viewport.
const PREFETCH_PAGES: usize = 2;

/// Decode at most this many images at once; each is a thread.
const MAX_CONCURRENT_IMAGES: usize = 4;
const MAX_CONCURRENT_BOOK_IMAGES: usize = 8;

/// Slot height before a page's real aspect ratio is known.
const SLOT_RATIO: f32 = 1.4;

const ACCENT: egui::Color32 = egui::Color32::from_gray(220);
const BACKGROUND: egui::Color32 = egui::Color32::from_gray(22);
const SURFACE: egui::Color32 = egui::Color32::from_gray(30);
const MUTED: egui::Color32 = egui::Color32::from_gray(150);
const FAINT: egui::Color32 = egui::Color32::from_gray(128);
const SEPARATOR: egui::Color32 = egui::Color32::from_gray(44);
const HOVER: egui::Color32 = egui::Color32::from_gray(44);
const DANGER: egui::Color32 = egui::Color32::from_rgb(228, 143, 143);

/// Target width of one cover in the browse grid.
const COVER_TARGET: f32 = 150.0;
/// How much a cover grows under the pointer, as a fraction of its size. Small
/// enough that the grid's spacing still keeps it clear of its neighbours.
const COVER_HOVER_GROWTH: f32 = 0.07;
/// Alpha of the neutral wash laid over a hovered series card.
const COVER_HOVER_WASH: u8 = 24;
/// Space between covers, and between rows.
const GRID_GAP: f32 = 16.0;
/// Height of one chapter row; the virtualised list depends on it.
const ROW_HEIGHT: f32 = 34.0;
/// Gap between pages in the paged reading modes.
const PAGE_GAP: f32 = 18.0;
/// Room kept free under a page that fills the window height.
const PAGE_MARGIN: f32 = 24.0;
/// Height of the reader toolbar, including its vertical padding.
const READER_BAR_HEIGHT: f32 = 40.0;
/// The reader's bar floats over the page and is pure black.
const READER_BAR_FILL: egui::Color32 = egui::Color32::BLACK;
/// A pointer within this many points of the bottom edge brings the bar back.
/// Twice the height of the bar plus a wide margin, so reaching for it is easy.
const READER_BAR_REVEAL: f32 = 112.0;
/// How long the pointer has to rest near the bottom edge first. A quick pass on
/// the way to a page should not bring the bar up; only a deliberate reach does.
const READER_BAR_REVEAL_DELAY: f64 = 0.3;
/// Once the toolbar is down, the reveal band grows by this much so it does not
/// snap away while the pointer is still near it.
const READER_BAR_REVEAL_GROWTH: f32 = 1.5;
/// The reader can shrink the window to the page; the rest of the UI still needs
/// this much width to lay out.
const MIN_WINDOW_WIDTH: f32 = 520.0;
/// Empty space narrower than this is not worth a window resize.
const SIDE_BAR_MIN: f32 = 20.0;
/// Below this the browse and series layouts stop fitting in the window.
const MIN_WINDOW_HEIGHT: f32 = 640.0;
/// Window size on a launch that has no saved size yet.
const DEFAULT_WINDOW_WIDTH: f32 = 1180.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 860.0;

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
enum ReadingMode {
    #[default]
    FitPage,
    FitWidth,
    Webtoon,
    MangaBook,
}

impl ReadingMode {
    fn label(self) -> &'static str {
        match self {
            Self::FitPage => "Fit page",
            Self::FitWidth => "Fit width",
            Self::Webtoon => "Webtoon",
            Self::MangaBook => "Manga Book",
        }
    }
}

/// Schema version of `reader.json`. Version 1 turned the page fit on by
/// default; older files cannot record a deliberate opt-out.
const READER_SETTINGS_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ReaderSettings {
    mode: ReadingMode,
    scale: f32,
    /// Shrink the window to the page so no empty bars sit beside it.
    fit_window: bool,
    /// Shrink the window height to the page so no empty band sits under it.
    fit_window_height: bool,
    /// Defaults to 0 when absent, which marks a file written before the field
    /// existed; `Default` writes the current version.
    #[serde(default)]
    version: u32,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            mode: ReadingMode::FitPage,
            scale: 1.0,
            fit_window: true,
            fit_window_height: true,
            version: READER_SETTINGS_VERSION,
        }
    }
}

impl ReaderSettings {
    /// Older settings never recorded the fit as a choice: their `false` is the
    /// old default, not the user's, so turn it on once and mark them current.
    fn upgrade(&mut self) -> bool {
        if self.version >= READER_SETTINGS_VERSION {
            return false;
        }
        self.fit_window = true;
        self.version = READER_SETTINGS_VERSION;
        true
    }
}

/// Window size remembered across launches. Kept apart from the reader settings
/// because the reader narrows the window to the page and that is not a size the
/// user chose.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct WindowState {
    width: f32,
    height: f32,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        }
    }
}

impl WindowState {
    fn load(root: &Path) -> Self {
        local::read::<Self>(&root.join("window.json"))
            .unwrap_or(None)
            .unwrap_or_default()
            .sanitized()
    }

    /// A corrupt file, or one saved on a bigger display, must not open a window
    /// too small for the UI.
    fn sanitized(self) -> Self {
        let dimension = |value: f32, min: f32, fallback: f32| {
            if value.is_finite() {
                value.max(min)
            } else {
                fallback
            }
        };
        Self {
            width: dimension(self.width, MIN_WINDOW_WIDTH, DEFAULT_WINDOW_WIDTH),
            height: dimension(self.height, MIN_WINDOW_HEIGHT, DEFAULT_WINDOW_HEIGHT),
        }
    }
}

#[derive(Default)]
enum ImageSlot {
    #[default]
    Empty,
    Loading,
    Ready(egui::TextureHandle),
    Failed(String),
}

/// Errors cross threads as plain strings; the sources carry their own detail.
type Outcome<T> = Result<T, String>;

struct DecodedPage {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

type OpeningPages = (Vec<Page>, Vec<Option<DecodedPage>>);
type PreparedOpening = (Vec<Page>, Vec<(usize, DecodedPage)>);
type ImageTiming = (bool, std::time::Duration, std::time::Duration);

enum Event {
    Popular(Outcome<MangaPage>),
    Latest(Outcome<MangaPage>),
    Search(Outcome<MangaPage>),
    Details(Outcome<Manga>),
    Chapters(ChapterList),
    ChapterRefreshDone(Outcome<()>),
    Pages(Outcome<Vec<Page>>),
    Image {
        index: usize,
        page: Outcome<DecodedPage>,
        timing: ImageTiming,
    },
    WarmChapter(String, Option<OpeningPages>),
    PreparedChapter(String, Outcome<PreparedOpening>),
    DownloadProgress(u64, Option<String>, String),
    DownloadFinished(Outcome<()>),
    Cover {
        url: String,
        image: Outcome<DecodedPage>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Browse,
    Series,
    Reader,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Popular,
    Latest,
    Downloads,
}

struct App {
    source: Arc<dyn Source>,
    source_options: Vec<Arc<dyn Source>>,
    tx: Sender<(u64, Event)>,
    rx: Receiver<(u64, Event)>,
    series_job: u64,
    view_job: u64,
    refreshing: bool,
    complete: bool,
    chapter_status: Option<String>,
    cancel_refresh: Arc<AtomicBool>,
    cancel_images: Arc<AtomicBool>,
    image_http: Option<Arc<rustga::HttpSource>>,
    image_in_flight: usize,
    hide_duplicates: bool,
    covers: HashMap<String, ImageSlot>,
    covers_in_flight: usize,
    reader_settings: ReaderSettings,
    data_root: PathBuf,
    current_page: usize,
    book_turn: Option<book::Turn>,
    book_drag: Option<(book::Turn, f32)>,
    saved_position: Option<(usize, bool)>,
    warm_chapter: Option<(String, OpeningPages)>,
    warming: Option<String>,
    book_pending_turn: Option<bool>,
    book_swipe: book::Swipe,
    book_cover_visible: bool,
    anchor_page: Option<usize>,
    reader_row_width: f32,
    reader_bar_visible: bool,
    reader_bar_slide: f32,
    /// When the pointer entered the toolbar's reveal band, so only a pointer
    /// that rests there brings the bar down.
    reader_bar_reveal_since: Option<f64>,
    /// The reader hides the title bar around the window's own buttons.
    borderless: bool,
    /// Axes the reader shrank to the page, restored on the way out.
    reader_shrunk_width: bool,
    reader_shrunk_height: bool,
    /// Size the user last left the window at, saved beside the reader settings.
    window: WindowState,
    /// Previous frame's size, so a drag is saved once it settles.
    window_observed: Option<egui::Vec2>,
    search_results: bool,

    screen: Screen,
    tab: Tab,
    query: String,
    status: String,
    error: Option<String>,
    busy: bool,

    mangas: Vec<Manga>,
    series: Option<Manga>,
    chapters: Vec<Chapter>,
    chapter: Option<Chapter>,
    pages: Vec<Page>,
    images: Vec<ImageSlot>,
    page_dimensions: Vec<Option<egui::Vec2>>,
    download_cancel: Option<Arc<AtomicBool>>,
    download_status: String,
    downloaded: std::collections::HashSet<(u64, String)>,
    ready_pages: HashMap<String, (egui::TextureHandle, u64)>,
    ready_clock: u64,
    prepared_manifests: HashMap<String, Vec<Page>>,
    preparing_chapter: Option<String>,
}

impl App {
    fn new_with_sources(
        source: Arc<dyn Source>,
        source_options: Vec<Arc<dyn Source>>,
        ctx: &egui::Context,
        window: WindowState,
    ) -> Self {
        configure_style(ctx);
        let (tx, rx) = mpsc::channel();
        let data_root = local::directory();
        let settings_path = data_root.join("reader.json");
        let mut reader_settings = local::read::<ReaderSettings>(&settings_path)
            .unwrap_or(None)
            .unwrap_or_default();
        let mut upgrade_error = None;
        if reader_settings.upgrade() {
            if let Err(error) = local::write(&settings_path, &reader_settings) {
                upgrade_error = Some(format!("Could not save reader settings: {error}"));
            }
        }
        reader_settings.scale = if reader_settings.scale.is_finite() {
            reader_settings.scale.clamp(0.4, 1.0)
        } else {
            1.0
        };
        let mut app = Self {
            source,
            source_options,
            tx,
            rx,
            series_job: 0,
            view_job: 0,
            refreshing: false,
            complete: false,
            chapter_status: None,
            cancel_refresh: Arc::new(AtomicBool::new(false)),
            cancel_images: Arc::new(AtomicBool::new(false)),
            image_http: rustga::HttpSource::new(100).ok().map(Arc::new),
            image_in_flight: 0,
            hide_duplicates: local::read(&data_root.join("hide-duplicates.json"))
                .unwrap_or(None)
                .unwrap_or(true),
            covers: HashMap::new(),
            covers_in_flight: 0,
            reader_settings,
            data_root,
            current_page: 0,
            book_turn: None,
            book_drag: None,
            saved_position: None,
            warm_chapter: None,
            warming: None,
            book_pending_turn: None,
            book_swipe: book::Swipe::default(),
            book_cover_visible: true,
            anchor_page: None,
            reader_row_width: 1.0,
            reader_bar_visible: false,
            reader_bar_slide: 0.0,
            reader_bar_reveal_since: None,
            borderless: false,
            reader_shrunk_width: false,
            reader_shrunk_height: false,
            window,
            window_observed: None,
            search_results: false,
            screen: Screen::Browse,
            tab: Tab::Popular,
            query: String::new(),
            status: String::new(),
            error: upgrade_error,
            busy: false,
            mangas: Vec::new(),
            series: None,
            chapters: Vec::new(),
            chapter: None,
            pages: Vec::new(),
            images: Vec::new(),
            page_dimensions: Vec::new(),
            download_cancel: None,
            download_status: String::new(),
            downloaded: std::collections::HashSet::new(),
            ready_pages: HashMap::new(),
            ready_clock: 0,
            prepared_manifests: HashMap::new(),
            preparing_chapter: None,
        };
        for source in &app.source_options {
            if let Ok(library) = downloads::library(&app.data_root, source.info().id) {
                for manga in library {
                    for chapter in manga.chapters {
                        app.downloaded.insert((source.info().id, chapter.url));
                    }
                }
            }
        }
        app.load_listing(ctx, Tab::Popular);
        app
    }

    /// Run a source call off the UI thread and wake the UI when it lands.
    fn spawn<F>(&self, ctx: &egui::Context, generation: u64, job: F)
    where
        F: FnOnce(Arc<dyn Source>) -> Event + Send + 'static,
    {
        let source = self.source.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let event = job(source);
            let _ = tx.send((generation, event));
            ctx.request_repaint();
        });
    }

    fn remember_texture(&mut self, url: String, texture: egui::TextureHandle) {
        self.ready_clock += 1;
        self.ready_pages.insert(url, (texture, self.ready_clock));
        while self.ready_pages.len() > 1
            && self
                .ready_pages
                .values()
                .map(|(t, _)| t.size()[0] * t.size()[1] * 4)
                .sum::<usize>()
                > 96 * 1024 * 1024
        {
            let oldest = self
                .ready_pages
                .iter()
                .min_by_key(|(_, (_, time))| *time)
                .map(|(url, _)| url.clone())
                .unwrap();
            self.ready_pages.remove(&oldest);
        }
    }

    fn prepare_download(&mut self, ctx: &egui::Context, chapter: Chapter) {
        if self.preparing_chapter.is_some()
            || !self
                .downloaded
                .contains(&(self.source.info().id, chapter.url.clone()))
        {
            return;
        }
        let page = local::read::<(usize, bool)>(&self.position_path(&chapter))
            .ok()
            .flatten()
            .map_or(0, |p| p.0);
        if self
            .prepared_manifests
            .get(&chapter.url)
            .is_some_and(|pages| {
                pages
                    .iter()
                    .skip(page)
                    .take(4)
                    .all(|p| self.ready_pages.contains_key(image_url(p)))
            })
        {
            return;
        }
        self.preparing_chapter = Some(chapter.url.clone());
        let root = self.data_root.clone();
        self.spawn(ctx, 0, move |source| {
            let result = (|| {
                let pages = downloads::pages(&root, source.info().id, &chapter.url)
                    .map_err(|e| e.to_string())?
                    .ok_or("Downloaded files are missing")?;
                let start = page.min(pages.len().saturating_sub(1));
                let mut decoded = Vec::new();
                for (index, page) in pages
                    .iter()
                    .enumerate()
                    .take(start + 4)
                    .skip(start.saturating_sub(1))
                {
                    let ready = downloads::prepare_page(&root, image_url(page))?;
                    decoded.push((
                        index,
                        DecodedPage {
                            width: ready.width,
                            height: ready.height,
                            pixels: ready.pixels,
                        },
                    ));
                }
                Ok((pages, decoded))
            })();
            Event::PreparedChapter(chapter.url, result)
        });
    }

    fn start_download(&mut self, ctx: &egui::Context, manga: Manga, chapters: Vec<Chapter>) {
        if self.download_cancel.is_some() {
            return;
        }
        let Some(http) = self.image_http.clone() else {
            self.download_status = "No image client".into();
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.download_cancel = Some(cancel.clone());
        self.download_status = "Starting download…".into();
        let root = self.data_root.clone();
        let tx = self.tx.clone();
        let repaint = ctx.clone();
        self.spawn(ctx, 0, move |source| {
            let result = downloads::download(
                &root,
                source.as_ref(),
                &http,
                &manga,
                &chapters,
                &cancel,
                |chapter, message| {
                    let _ = tx.send((
                        0,
                        Event::DownloadProgress(
                            source.info().id,
                            chapter.map(str::to_owned),
                            message,
                        ),
                    ));
                    repaint.request_repaint();
                },
            );
            Event::DownloadFinished(result)
        });
    }

    fn load_listing(&mut self, ctx: &egui::Context, tab: Tab) {
        self.screen = Screen::Browse;
        self.search_results = false;
        self.view_job += 1;
        self.tab = tab;
        self.busy = true;
        self.error = None;
        self.status = "Loading…".to_owned();
        if tab == Tab::Downloads {
            self.busy = false;
            self.status.clear();
            match downloads::library(&self.data_root, self.source.info().id) {
                Ok(library) => self.mangas = library.into_iter().map(|entry| entry.manga).collect(),
                Err(e) => self.error = Some(format!("Could not read downloads: {e}")),
            }
            return;
        }
        self.spawn(ctx, self.view_job, move |source| match tab {
            Tab::Popular => Event::Popular(page(source.popular(1))),
            Tab::Latest => Event::Latest(page(source.latest(1))),
            Tab::Downloads => unreachable!(),
        });
    }

    fn run_search(&mut self, ctx: &egui::Context) {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.search_results = true;
        self.view_job += 1;
        self.busy = true;
        self.error = None;
        self.status = format!("Searching “{query}”…");
        self.spawn(ctx, self.view_job, move |source| {
            Event::Search(page(source.search(&query, 1, &FilterValues::new())))
        });
    }

    fn switch_source(&mut self, ctx: &egui::Context, source: Arc<dyn Source>) {
        if source.info().id == self.source.info().id {
            return;
        }
        self.cancel_refresh.store(true, Ordering::Relaxed);
        self.cancel_images.store(true, Ordering::Relaxed);
        self.source = source;
        self.series_job += 1;
        self.view_job += 1;
        self.cancel_refresh = Arc::new(AtomicBool::new(false));
        self.cancel_images = Arc::new(AtomicBool::new(false));
        self.series = None;
        self.chapter = None;
        self.chapters.clear();
        self.pages.clear();
        self.images.clear();
        self.mangas.clear();
        self.query.clear();
        self.search_results = false;
        self.screen = Screen::Browse;
        self.tab = Tab::Popular;
        self.load_listing(ctx, Tab::Popular);
    }

    fn open_series(&mut self, ctx: &egui::Context, manga: Manga) {
        self.cancel_refresh.store(true, Ordering::Relaxed);
        self.screen = Screen::Series;
        self.series = Some(manga.clone());
        self.series_job += 1;
        self.view_job += 1;
        self.cancel_refresh = Arc::new(AtomicBool::new(false));
        self.chapters.clear();
        self.complete = false;
        self.refreshing = true;
        self.chapter_status = None;
        self.busy = false;
        self.error = None;

        if self.tab == Tab::Downloads && !self.search_results {
            match downloads::library(&self.data_root, self.source.info().id) {
                Ok(library) => {
                    if let Some(saved) = library
                        .into_iter()
                        .find(|entry| entry.manga.url == manga.url)
                    {
                        self.series = Some(saved.manga);
                        self.chapters = saved.chapters;
                        self.complete = true;
                    }
                }
                Err(e) => self.error = Some(e.to_string()),
            }
            self.refreshing = false;
            return;
        }
        let details = manga.clone();
        self.spawn(ctx, self.series_job, move |source| {
            Event::Details(source.details(&details).map_err(|error| error.to_string()))
        });
        let generation = self.series_job;
        let series = reqwest::Url::parse(&self.source.info().base_url)
            .and_then(|url| url.join(&manga.url))
            .map_or_else(|_| manga.url.clone(), |url| url.to_string());
        let path = local::chapter_path(&self.data_root, self.source.info().id, &series);
        let cancel = self.cancel_refresh.clone();
        let tx = self.tx.clone();
        let repaint = ctx.clone();
        self.spawn(ctx, generation, move |source| {
            let emit = |list: &ChapterList| {
                let sent = tx.send((generation, Event::Chapters(list.clone()))).is_ok();
                repaint.request_repaint();
                sent
            };
            let cached = local::read::<ChapterList>(&path)
                .unwrap_or(None)
                .unwrap_or_default();
            if !cached.chapters.is_empty() {
                emit(&cached);
            }
            let mut latest = None;
            let result = source.refresh_chapters(&manga, cached, &mut |list| {
                // A snapshot of the same size can still differ: the source may
                // have replaced a chapter. Comparing only the count would show
                // and save the stale list, keeping removed chapters on disk.
                if latest.as_ref().is_none_or(|old: &ChapterList| {
                    old.chapters != list.chapters || old.complete != list.complete
                }) {
                    emit(list);
                    latest = Some(list.clone());
                }
                !cancel.load(Ordering::Relaxed)
            });
            let saved = latest.map_or(Ok(()), |list| local::save_chapters(&path, &list));
            Event::ChapterRefreshDone(
                result
                    .map_err(|e| e.to_string())
                    .and(saved.map_err(|e| format!("Could not save chapter list: {e}"))),
            )
        });
    }

    fn open_chapter(&mut self, ctx: &egui::Context, chapter: Chapter) {
        self.save_position();
        self.saved_position = local::read(&self.position_path(&chapter)).ok().flatten();
        let (page, cover) = self.saved_position.unwrap_or((0, true));
        self.current_page = page;
        self.book_turn = None;
        self.book_drag = None;
        self.book_pending_turn = None;
        self.book_swipe = book::Swipe::default();
        self.book_cover_visible = cover;
        self.screen = Screen::Reader;
        self.chapter = Some(chapter.clone());
        self.pages.clear();
        self.images.clear();
        self.anchor_page = (page != 0).then_some(page);
        // Readings start clean: the toolbar slides away until the pointer returns.
        self.reader_bar_visible = false;
        self.reader_bar_reveal_since = None;
        self.view_job += 1;
        self.cancel_images.store(true, Ordering::Relaxed);
        self.cancel_images = Arc::new(AtomicBool::new(false));
        self.image_in_flight = 0;
        self.busy = true;
        self.error = None;
        self.status = "Loading chapter…".to_owned();
        self.warming = None;
        if let Some((url, (pages, decoded))) = self.warm_chapter.take() {
            if url == chapter.url {
                self.accept_pages(pages);
                for (index, image) in decoded.into_iter().enumerate() {
                    if let Some(image) = image {
                        self.page_dimensions[index] =
                            Some(egui::vec2(image.width as f32, image.height as f32));
                        self.images[index] = ImageSlot::Ready(page_texture(ctx, index, image));
                    }
                }
                return;
            }
        }
        if let Some(pages) = self.prepared_manifests.get(&chapter.url).cloned() {
            self.accept_pages(pages);
            return;
        }
        let root = self.data_root.clone();
        self.spawn(ctx, self.view_job, move |source| {
            let pages = downloads::pages(&root, source.info().id, &chapter.url)
                .map_err(|e| e.to_string())
                .and_then(|saved| {
                    saved.map_or_else(|| source.pages(&chapter).map_err(|e| e.to_string()), Ok)
                });
            Event::Pages(match pages {
                Ok(pages) => Ok(pages),
                Err(error) => Err(error.to_string()),
            })
        });
    }

    /// Fetch one page image, but only while the page is still wanted.
    fn load_image(&mut self, index: usize, ctx: &egui::Context) {
        let Some(slot) = self.images.get(index) else {
            return;
        };
        let limit = if self.reader_settings.mode == ReadingMode::MangaBook {
            MAX_CONCURRENT_BOOK_IMAGES
        } else {
            MAX_CONCURRENT_IMAGES
        };
        if !matches!(slot, ImageSlot::Empty) || self.image_in_flight >= limit {
            return;
        }
        if let Some((texture, time)) = self.ready_pages.get_mut(image_url(&self.pages[index])) {
            self.ready_clock += 1;
            *time = self.ready_clock;
            self.page_dimensions[index] = Some(texture.size_vec2());
            self.images[index] = ImageSlot::Ready(texture.clone());
            return;
        }
        let Some(http) = self.image_http.clone() else {
            self.images[index] = ImageSlot::Failed("No image client".to_owned());
            return;
        };
        let url = image_url(&self.pages[index]).to_owned();
        let cache = self.data_root.join("page-cache");
        let generation = self.view_job;
        let image_width = (ctx.screen_rect().width() * ctx.pixels_per_point())
            .clamp(PAGE_WIDTH as f32, 2400.0) as u32;
        let cancel = self.cancel_images.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();

        self.images[index] = ImageSlot::Loading;
        self.image_in_flight += 1;
        thread::spawn(move || {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let (page, timing) = fetch_page(&http, &cache, &url, &cancel, image_width);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send((
                generation,
                Event::Image {
                    index,
                    page,
                    timing,
                },
            ));
            ctx.request_repaint();
        });
    }

    fn back(&mut self, ctx: &egui::Context) {
        match self.screen {
            Screen::Reader => {
                self.save_position();
                self.warming = None;
                self.warm_chapter = None;
                self.cancel_images.store(true, Ordering::Relaxed);
                self.view_job += 1;
                self.image_in_flight = 0;
                // The page load just cancelled will never report back, so the
                // busy flag it raised has to be lowered here.
                self.busy = false;
                self.restore_window(ctx);
                self.screen = Screen::Series;
            }
            Screen::Series => {
                self.cancel_refresh.store(true, Ordering::Relaxed);
                self.series_job += 1;
                self.screen = Screen::Browse;
                self.series = None;
            }
            Screen::Browse => {}
        }
    }

    /// Move through the deduplicated list: `+1` is the next chapter, which is
    /// one row earlier because the list runs newest first.
    fn jump_chapter(&mut self, ctx: &egui::Context, step: isize) {
        let Some(current) = self.chapter.as_ref() else {
            return;
        };
        let order = local::ordered_indices(&self.chapters, self.hide_duplicates);
        let Some(position) = order
            .iter()
            .position(|&index| self.chapters[index].url == current.url)
        else {
            return;
        };
        let Some(target) = (position as isize).checked_sub(step).filter(|t| *t >= 0) else {
            return;
        };
        let Some(&index) = order.get(target as usize) else {
            return;
        };
        let chapter = self.chapters[index].clone();
        self.open_chapter(ctx, chapter);
    }

    fn position_path(&self, chapter: &Chapter) -> PathBuf {
        self.data_root.join(format!(
            "position-{:016x}.json",
            local::url_key(chapter.url.trim_end_matches('/'))
        ))
    }

    fn save_position(&mut self) {
        if self.screen != Screen::Reader || self.pages.is_empty() || self.anchor_page.is_some() {
            return;
        }
        let position = (self.current_page, self.book_cover_visible);
        if self.saved_position != Some(position) {
            if let Some(chapter) = &self.chapter {
                match local::write(&self.position_path(chapter), &position) {
                    Ok(()) => self.saved_position = Some(position),
                    Err(_) => self.error = Some("Could not save reading position".to_owned()),
                }
            }
        }
    }

    fn accept_pages(&mut self, pages: Vec<Page>) {
        self.busy = false;
        self.current_page = self.current_page.min(pages.len().saturating_sub(1));
        self.anchor_page = self.anchor_page.map(|_| self.current_page);
        self.status.clear();
        self.page_dimensions = vec![None; pages.len()];
        self.pages = pages;
        self.images = self
            .pages
            .iter()
            .enumerate()
            .map(|(index, page)| {
                if let Some((texture, _)) = self.ready_pages.get(image_url(page)) {
                    self.page_dimensions[index] = Some(texture.size_vec2());
                    ImageSlot::Ready(texture.clone())
                } else {
                    ImageSlot::Empty
                }
            })
            .collect();
    }

    fn warm_next_chapter(&mut self, ctx: &egui::Context) {
        if self.screen != Screen::Reader
            || self.pages.is_empty()
            || self.warming.is_some()
            || self.image_in_flight != 0
        {
            return;
        }
        let (Some(current), Some(http)) = (&self.chapter, self.image_http.clone()) else {
            return;
        };
        let order = local::ordered_indices(&self.chapters, self.hide_duplicates);
        let Some(next) = order.windows(2).find_map(|pair| {
            (self.chapters[pair[1]].url == current.url).then(|| self.chapters[pair[0]].clone())
        }) else {
            return;
        };
        if self.current_page.saturating_add(4) < self.pages.len()
            && !self
                .downloaded
                .contains(&(self.source.info().id, next.url.clone()))
        {
            return;
        }
        self.warming = Some(next.url.clone());
        let cache = self.data_root.join("page-cache");
        let image_width = (ctx.screen_rect().width() * ctx.pixels_per_point())
            .clamp(PAGE_WIDTH as f32, 2400.0) as u32;
        let cancel = self.cancel_images.clone();
        let root = self.data_root.clone();
        self.spawn(ctx, self.view_job, move |source| {
            let pages = downloads::pages(&root, source.info().id, &next.url)
                .ok()
                .flatten()
                .or_else(|| source.pages(&next).ok());
            let result = pages.map(|pages| {
                let images = pages
                    .iter()
                    .take(2)
                    .take_while(|_| !cancel.load(Ordering::Relaxed))
                    .map(|page| {
                        fetch_page(&http, &cache, image_url(page), &cancel, image_width)
                            .0
                            .ok()
                    })
                    .collect();
                (pages, images)
            });
            Event::WarmChapter(next.url, result)
        });
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok((generation, event)) = self.rx.try_recv() {
            if !current_event(&event, generation, self.series_job, self.view_job) {
                continue;
            }
            match event {
                Event::Popular(result) | Event::Latest(result) => match result {
                    Ok(page) => {
                        self.mangas = page.mangas;
                        self.busy = false;
                        self.status.clear();
                    }
                    Err(error) => {
                        self.busy = false;
                        self.error = Some(error);
                    }
                },
                Event::Search(result) => {
                    self.busy = false;
                    match result {
                        Ok(page) => {
                            self.mangas = page.mangas;
                            self.status = format!("{} results", self.mangas.len());
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Event::Details(result) => {
                    if let Ok(manga) = result {
                        self.series = Some(manga);
                    }
                }
                Event::Chapters(list) => {
                    self.chapters = list.chapters;
                    self.complete = list.complete;
                }
                Event::ChapterRefreshDone(result) => {
                    self.refreshing = false;
                    self.chapter_status = result.err();
                }
                Event::Pages(result) => {
                    self.busy = false;
                    match result {
                        Ok(pages) => {
                            self.accept_pages(pages);
                            self.load_image(self.current_page, ctx);
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Event::WarmChapter(url, result) => {
                    if let Some(opening) = result {
                        self.warm_chapter = Some((url, opening));
                    }
                }
                Event::Image {
                    index,
                    page,
                    timing: (cached, fetch, decode),
                } => {
                    self.image_in_flight = self.image_in_flight.saturating_sub(1);
                    let Some(slot) = self.images.get_mut(index) else {
                        continue;
                    };
                    *slot = match page {
                        Ok(decoded) => {
                            let started = std::time::Instant::now();
                            self.page_dimensions[index] =
                                Some(egui::vec2(decoded.width as f32, decoded.height as f32));
                            let texture = page_texture(ctx, index, decoded);
                            if std::env::var_os("RUSTGA_IMAGE_TIMING").is_some() {
                                eprintln!("page {}: cache={cached}, fetch={fetch:?}, decode={decode:?}, texture staging={:?}", index + 1, started.elapsed());
                            }
                            ImageSlot::Ready(texture)
                        }
                        Err(error) => ImageSlot::Failed(error),
                    };
                    if let ImageSlot::Ready(texture) = &self.images[index] {
                        self.remember_texture(
                            image_url(&self.pages[index]).to_owned(),
                            texture.clone(),
                        );
                    }
                }
                Event::PreparedChapter(chapter, result) => {
                    self.preparing_chapter = None;
                    if let Ok((pages, decoded)) = result {
                        for (index, image) in decoded {
                            let texture = page_texture(ctx, index, image);
                            let url = image_url(&pages[index]).to_owned();
                            if let Some(current) =
                                self.pages.iter().position(|page| image_url(page) == url)
                            {
                                self.page_dimensions[current] = Some(texture.size_vec2());
                                self.images[current] = ImageSlot::Ready(texture.clone());
                            }
                            self.remember_texture(url, texture);
                        }
                        if self.prepared_manifests.len() >= 8 {
                            self.prepared_manifests.clear();
                        }
                        self.prepared_manifests.insert(chapter, pages);
                    }
                }
                Event::DownloadProgress(source, chapter, message) => {
                    if let Some(chapter) = chapter {
                        self.downloaded.insert((source, chapter));
                    }
                    self.download_status = message;
                }
                Event::DownloadFinished(result) => {
                    self.download_cancel = None;
                    self.download_status = match result {
                        Ok(()) => "Download complete".into(),
                        Err(e) => format!("Download stopped: {e}"),
                    };
                }
                Event::Cover { url, image } => {
                    self.covers_in_flight = self.covers_in_flight.saturating_sub(1);
                    let slot = match image {
                        Ok(decoded) => ImageSlot::Ready(ctx.load_texture(
                            &url,
                            egui::ColorImage::from_rgba_unmultiplied(
                                [decoded.width, decoded.height],
                                &decoded.pixels,
                            ),
                            egui::TextureOptions::LINEAR,
                        )),
                        Err(error) => ImageSlot::Failed(error),
                    };
                    self.covers.insert(url, slot);
                }
            }
        }
    }

    fn cover(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        url: Option<&str>,
        ctx: &egui::Context,
    ) {
        ui.painter().rect_filled(rect, 8.0, SURFACE);
        let Some(url) = url else { return };
        if let Some(ImageSlot::Ready(texture)) = self.covers.get(url) {
            egui::Image::new(texture)
                .corner_radius(8.0)
                .paint_at(ui, rect);
            return;
        }
        if let Some(ImageSlot::Failed(message)) = self.covers.get(url) {
            let response = ui.interact(rect, ui.id().with(url), egui::Sense::click());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Retry cover",
                egui::FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );
            if response.on_hover_text(message).clicked() {
                self.covers.remove(url);
            }
            return;
        }
        if ui.is_rect_visible(rect) {
            self.load_cover(url, ctx);
        }
    }

    fn load_cover(&mut self, url: &str, ctx: &egui::Context) {
        if self.covers.contains_key(url) || self.covers_in_flight >= 4 {
            return;
        }
        let Some(http) = self.image_http.clone() else {
            return;
        };
        let url = url.to_owned();
        self.covers.insert(url.clone(), ImageSlot::Loading);
        self.covers_in_flight += 1;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let root = self.data_root.clone();
        thread::spawn(move || {
            let image = std::fs::read(downloads::page_path(&root, &url))
                .map_err(|e| e.to_string())
                .or_else(|_| http.get_bytes(&url).map_err(|e| e.to_string()))
                .and_then(|bytes| decode_sized(&bytes, 900, 1350));
            let _ = tx.send((0, Event::Cover { url, image }));
            ctx.request_repaint();
        });
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Rustga").size(15.0).strong());
            ui.add_space(14.0);
            for (tab, name) in [
                (Tab::Popular, "Popular"),
                (Tab::Latest, "Latest"),
                (Tab::Downloads, "Downloads"),
            ] {
                let selected = !self.search_results && self.tab == tab;
                if ui
                    .selectable_label(selected, egui::RichText::new(name).size(13.0))
                    .clicked()
                    && !selected
                {
                    self.cancel_refresh.store(true, Ordering::Relaxed);
                    self.series_job += 1;
                    self.load_listing(ctx, tab);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let response = ui.add_sized(
                    [210.0, 26.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text("Search"),
                );
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked = ui
                    .add_enabled(
                        !self.busy && !self.query.trim().is_empty(),
                        egui::Button::new(egui::RichText::new("Search").size(13.0)),
                    )
                    .clicked();
                if enter || clicked {
                    self.run_search(ctx);
                }
                ui.add_space(6.0);
                let source = self.source.info().name.clone();
                let options = self.source_options.clone();
                let mut selected = None;
                ui.menu_button(egui::RichText::new(&source).size(12.0), |ui| {
                    for option in &options {
                        let selected_now = option.info().id == self.source.info().id;
                        if ui
                            .selectable_label(selected_now, &option.info().name)
                            .clicked()
                        {
                            selected = Some(option.clone());
                        }
                    }
                });
                if let Some(source) = selected {
                    self.switch_source(ctx, source);
                }
            });
        });
    }

    fn browse(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let (heading, detail) = if self.search_results {
                (
                    format!("Results for “{}”", self.query.trim()),
                    format!("{} found", self.mangas.len()),
                )
            } else {
                (
                    if self.tab == Tab::Downloads {
                        "Downloads".to_owned()
                    } else if self.tab == Tab::Popular {
                        "Popular".to_owned()
                    } else {
                        "Latest".to_owned()
                    },
                    format!("{} series", self.mangas.len()),
                )
            };
            ui.label(egui::RichText::new(heading).size(13.0).strong());
            if !self.mangas.is_empty() {
                ui.label(egui::RichText::new(detail).size(12.0).color(MUTED));
            }
            if self.search_results
                && ui
                    .add(egui::Button::new(egui::RichText::new("Clear").size(12.0)).frame(false))
                    .clicked()
            {
                self.load_listing(ctx, self.tab);
            }
            if self.busy {
                ui.spinner();
            }
        });
        if let Some(error) = &self.error {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(error).size(12.0).color(DANGER));
        }
        ui.add_space(12.0);
        if self.mangas.is_empty() {
            if !self.busy {
                ui.label(
                    egui::RichText::new("No titles found.")
                        .size(13.0)
                        .color(MUTED),
                );
            }
            return;
        }

        let available = ui.available_width();
        let columns =
            (((available + GRID_GAP) / (COVER_TARGET + GRID_GAP)).floor() as usize).max(2);
        let width = ((available - GRID_GAP * (columns - 1) as f32) / columns as f32).max(90.0);
        let cover_height = (width * 1.45).round();
        let mut chosen = None;
        egui::ScrollArea::vertical()
            .id_salt((
                "browse",
                self.tab as u8,
                self.search_results,
                self.query.as_str(),
            ))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(GRID_GAP, 20.0);
                for row in (0..self.mangas.len()).step_by(columns) {
                    ui.horizontal_top(|ui| {
                        for index in row..(row + columns).min(self.mangas.len()) {
                            let manga = self.mangas[index].clone();
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(width, cover_height + 42.0),
                                egui::Sense::click(),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    &manga.title,
                                )
                            });
                            // Hover eases in a little growth about the cover's
                            // centre and a neutral wash over the whole card, so a
                            // series reacts instead of snapping on. One animation
                            // per series keeps the grid from pulsing in unison.
                            let hover = ease_out_exponential(ctx.animate_bool_with_time(
                                egui::Id::new(("cover-hover", &manga.url)),
                                response.hovered(),
                                0.18,
                            ));
                            let settled = egui::Rect::from_min_size(
                                rect.min,
                                egui::vec2(width, cover_height),
                            );
                            let cover_rect = egui::Rect::from_center_size(
                                settled.center(),
                                settled.size() * (1.0 + COVER_HOVER_GROWTH * hover),
                            );
                            self.cover(ui, cover_rect, manga.thumbnail_url.as_deref(), ctx);
                            if hover > 0.0 {
                                let alpha = (COVER_HOVER_WASH as f32 * hover).round() as u8;
                                ui.painter().rect_filled(
                                    rect,
                                    8.0,
                                    egui::Color32::from_white_alpha(alpha),
                                );
                            }
                            let title_color =
                                egui::Color32::from_gray((206.0 + 40.0 * hover).round() as u8);
                            let title_rect = egui::Rect::from_min_size(
                                settled.left_bottom() + egui::vec2(0.0, 8.0),
                                egui::vec2(width, 34.0),
                            );
                            ui.put(
                                title_rect,
                                egui::Label::new(two_line_title(&manga.title, width, title_color)),
                            );
                            if response
                                .on_hover_text(&manga.title)
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                chosen = Some(manga);
                            }
                        }
                    });
                }
            });
        if let Some(manga) = chosen {
            self.open_series(ctx, manga);
        }
    }

    fn series_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(series) = self.series.clone() else {
            return;
        };
        let order = local::ordered_indices(&self.chapters, self.hide_duplicates);
        let height = ui.available_height();
        let left_width = (ui.available_width() * 0.32).clamp(260.0, 340.0);
        let mut hovered_chapter = None;
        let mut chosen = None;
        let mut go_back = false;
        let mut requested_download = Vec::new();

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(left_width, height),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    if ui
                        .add(egui::Button::new(egui::RichText::new("Back").size(13.0)).frame(false))
                        .clicked()
                    {
                        go_back = true;
                    }
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("series_details", self.series_job))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let cover_width = (left_width - 24.0).min(186.0);
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(cover_width, cover_width * 1.45),
                                egui::Sense::hover(),
                            );
                            self.cover(ui, rect, series.thumbnail_url.as_deref(), ctx);
                            ui.add_space(14.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&series.title).size(19.0).strong(),
                                )
                                .wrap(),
                            );
                            if let Some(author) =
                                series.author.as_deref().filter(|a| !a.trim().is_empty())
                            {
                                ui.add_space(3.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(author).size(12.5).color(MUTED),
                                    )
                                    .wrap(),
                                );
                            }
                            if let Some(genres) =
                                series.genre.as_deref().filter(|g| !g.trim().is_empty())
                            {
                                ui.add_space(6.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(genres).size(11.5).color(FAINT),
                                    )
                                    .wrap(),
                                );
                            }
                            ui.add_space(14.0);
                            let first = order
                                .iter()
                                .rev()
                                .copied()
                                .find(|&index| self.chapters[index].chapter_number.is_some());
                            if ui
                                .add_enabled(
                                    first.is_some(),
                                    egui::Button::new(
                                        egui::RichText::new("Start reading")
                                            .size(13.0)
                                            .color(BACKGROUND),
                                    )
                                    .fill(ACCENT),
                                )
                                .clicked()
                            {
                                chosen = first.map(|index| self.chapters[index].clone());
                            }
                            ui.add_space(8.0);
                            if ui.add_enabled(!order.is_empty() && !self.refreshing && self.complete && self.download_cancel.is_none(), egui::Button::new("Download all chapters"))
                                .on_hover_text("Download the listed chapters for offline reading after the chapter list finishes loading. Duplicate-release filtering applies.")
                                .clicked() {
                                requested_download = order.iter().rev().map(|&i| self.chapters[i].clone()).collect();
                            }
                            if !self.download_status.is_empty() { ui.label(egui::RichText::new(&self.download_status).small()); }
                            if let Some(cancel) = &self.download_cancel {
                                if ui.button("Cancel download").clicked() { cancel.store(true, Ordering::Relaxed); }
                            }
                            if let Some(description) = series
                                .description
                                .as_deref()
                                .filter(|description| !description.trim().is_empty())
                            {
                                ui.add_space(12.0);
                                egui::CollapsingHeader::new(
                                    egui::RichText::new("Description").size(12.5),
                                )
                                .id_salt(("description", self.series_job))
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(description)
                                                .size(12.5)
                                                .color(egui::Color32::from_gray(178)),
                                        )
                                        .wrap(),
                                    );
                                });
                            }
                        });
                },
            );
            ui.add_space(18.0);
            ui.separator();
            ui.add_space(18.0);

            let right_width = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(right_width, height),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}",
                                order.len(),
                                if self.hide_duplicates {
                                    "chapters"
                                } else {
                                    "releases"
                                }
                            ))
                            .size(13.0)
                            .strong(),
                        );
                        if self.refreshing {
                            ui.spinner();
                        } else if !self.complete && !order.is_empty() {
                            ui.label(egui::RichText::new("incomplete").size(11.5).color(MUTED));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .selectable_label(
                                    self.hide_duplicates,
                                    egui::RichText::new("Hide duplicate releases").size(12.0),
                                )
                                .clicked()
                            {
                                self.hide_duplicates = !self.hide_duplicates;
                                if let Err(error) = local::write(
                                    &self.data_root.join("hide-duplicates.json"),
                                    &self.hide_duplicates,
                                ) {
                                    self.chapter_status =
                                        Some(format!("Could not save preference: {error}"));
                                }
                            }
                        });
                    });
                    if let Some(message) = &self.chapter_status {
                        ui.label(egui::RichText::new(message).size(11.5).color(DANGER));
                    }
                    ui.add_space(4.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    egui::ScrollArea::vertical()
                        .id_salt(("chapters", self.series_job))
                        .auto_shrink([false, false])
                        .max_height(ui.available_height())
                        .show_rows(ui, ROW_HEIGHT, order.len(), |ui, range| {
                            for row in range {
                                let chapter = &self.chapters[order[row]];
                                ui.horizontal(|ui| {
                                    let done = self.downloaded.contains(&(self.source.info().id, chapter.url.clone()));
                                    if ui.add_enabled(self.download_cancel.is_none(), egui::Button::new(if done { "Downloaded" } else { "Download" }).min_size(egui::vec2(90.0, ROW_HEIGHT - 4.0)))
                                        .on_hover_text(if done { "Check or resume this chapter's download" } else { "Download chapter for offline reading" }).clicked() {
                                        requested_download = vec![chapter.clone()];
                                    }
                                    let response = chapter_row(ui, chapter);
                                    if response.hovered() { hovered_chapter = Some(chapter.clone()); }
                                    if response.clicked() { chosen = Some(chapter.clone()); }
                                });
                            }
                        });
                },
            );
        });

        if let Some(chapter) = hovered_chapter.or_else(|| {
            order
                .iter()
                .rev()
                .map(|&i| &self.chapters[i])
                .find(|c| {
                    self.downloaded
                        .contains(&(self.source.info().id, c.url.clone()))
                })
                .cloned()
        }) {
            self.prepare_download(ctx, chapter);
        }
        if !requested_download.is_empty() {
            self.start_download(ctx, series, requested_download);
        }
        if go_back {
            self.back(ctx);
        }
        if let Some(chapter) = chosen {
            self.open_chapter(ctx, chapter);
        }
    }

    fn reader_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (has_previous, has_next) = self.chapter_neighbours();
        let mut step = 0isize;
        let mut go_back = false;
        let mut settings_changed = false;

        ui.horizontal(|ui| {
            // The window's own buttons sit over the bar's left end.
            #[cfg(target_os = "macos")]
            ui.add_space(56.0);
            if ui
                .add(egui::Button::new(egui::RichText::new("Back").size(13.0)).frame(false))
                .clicked()
            {
                go_back = true;
            }
            ui.add_space(10.0);
            let book = self.reader_settings.mode == ReadingMode::MangaBook;
            let reserved = if book { 640.0 } else { 360.0 };
            let title_width = (ui.available_width() - reserved).max(90.0);
            ui.add_sized(
                [title_width, 24.0],
                egui::Label::new(egui::RichText::new(self.chapter_title()).size(13.0)).truncate(),
            );
            if book {
                ui.add_space(8.0);
                self.book_bar_controls(ui, ctx);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        has_next,
                        egui::Button::new(egui::RichText::new("Next").size(12.5)),
                    )
                    .clicked()
                {
                    step = 1;
                }
                if ui
                    .add_enabled(
                        has_previous,
                        egui::Button::new(egui::RichText::new("Previous").size(12.5)),
                    )
                    .clicked()
                {
                    step = -1;
                }
                if !self.pages.is_empty() && self.reader_settings.mode != ReadingMode::MangaBook {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            (self.current_page + 1).min(self.pages.len()),
                            self.pages.len()
                        ))
                        .size(11.5)
                        .color(MUTED),
                    );
                }
                let settings = ui.add(egui::Button::new(
                    egui::RichText::new("Reader settings").size(12.5),
                ));
                egui::Popup::from_toggle_button_response(&settings)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .gap(6.0)
                    .show(|ui| {
                        ui.set_min_width(250.0);
                        ui.horizontal_wrapped(|ui| {
                            for mode in [
                                ReadingMode::FitPage,
                                ReadingMode::FitWidth,
                                ReadingMode::Webtoon,
                                ReadingMode::MangaBook,
                            ] {
                                settings_changed |= ui
                                    .selectable_value(
                                        &mut self.reader_settings.mode,
                                        mode,
                                        mode.label(),
                                    )
                                    .changed();
                            }
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Size").size(12.0).color(MUTED));
                            let fit_window = self.reader_settings.fit_window
                                && self.reader_settings.mode != ReadingMode::MangaBook;
                            settings_changed |= ui
                                .add_enabled(
                                    !fit_window,
                                    egui::Slider::new(&mut self.reader_settings.scale, 0.4..=1.0)
                                        .show_value(false),
                                )
                                .changed();
                            let shown_size = if fit_window {
                                1.0
                            } else {
                                self.reader_settings.scale
                            };
                            ui.label(
                                egui::RichText::new(format!("{:.0}%", shown_size * 100.0))
                                    .size(11.5)
                                    .color(MUTED),
                            );
                        });
                        ui.add_space(8.0);
                        settings_changed |= ui
                            .add_enabled(
                                self.reader_settings.mode != ReadingMode::MangaBook,
                                egui::Checkbox::new(
                                    &mut self.reader_settings.fit_window,
                                    egui::RichText::new("Fit window to page").size(12.0),
                                ),
                            )
                            .on_disabled_hover_text("Manga Book fits the spread inside the window")
                            .changed();
                        settings_changed |= ui
                            .add_enabled(
                                self.reader_settings.mode != ReadingMode::MangaBook,
                                egui::Checkbox::new(
                                    &mut self.reader_settings.fit_window_height,
                                    egui::RichText::new("Fit window height to page").size(12.0),
                                ),
                            )
                            .changed();
                    });
            });
        });

        if settings_changed {
            self.anchor_page = Some(self.current_page);
            if let Err(error) =
                local::write(&self.data_root.join("reader.json"), &self.reader_settings)
            {
                self.error = Some(format!("Could not save reader settings: {error}"));
            }
        }
        if step != 0 {
            self.jump_chapter(ctx, step);
        }
        if go_back {
            self.back(ctx);
        }
    }

    fn reader(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Some(error) = &self.error {
            ui.label(egui::RichText::new(error).size(12.5).color(DANGER));
        }
        if self.busy {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
            return;
        }
        if self.pages.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("This chapter has no pages.").color(MUTED));
            });
            return;
        }

        let mode = self.reader_settings.mode;
        if mode == ReadingMode::MangaBook {
            self.restore_window(ctx);
            self.book_reader(ui, ctx);
            return;
        }
        self.book_turn = None;
        self.book_drag = None;
        self.book_pending_turn = None;
        self.book_swipe = book::Swipe::default();
        // Fitting the window to the page only makes sense at full size: a page
        // drawn smaller than the fit would leave side bars inside the window.
        let scale = if self.reader_settings.fit_window {
            1.0
        } else {
            self.reader_settings.scale
        };
        let viewport = ui.available_size();
        let mut retry = None;
        let mut wanted = Vec::new();
        let mut first_visible = None;
        let mut row_width = viewport.x;

        let mut area = egui::ScrollArea::vertical()
            .id_salt(("reader", self.view_job))
            .auto_shrink([false, false]);
        if let Some(page) = self.anchor_page.take() {
            area = area.vertical_scroll_offset(self.page_offset(
                page,
                self.reader_row_width.max(1.0),
                viewport,
                mode,
                scale,
            ));
        }
        area.show_viewport(ui, |ui, _| {
            // Paint into exact rectangles: no widget spacing, borders or rounding at seams.
            ui.spacing_mut().item_spacing.y = 0.0;
            let clip = ui.clip_rect();
            let preload = clip.expand2(egui::vec2(0.0, viewport.y * PREFETCH_PAGES as f32));
            row_width = ui.available_width();
            for index in 0..self.pages.len() {
                let size = self.page_size_at(index, row_width, viewport, mode, scale);
                let size = egui::vec2(
                    size.x,
                    (size.y * ctx.pixels_per_point()).round() / ctx.pixels_per_point(),
                );
                let (row, _) =
                    ui.allocate_exact_size(egui::vec2(row_width, size.y), egui::Sense::hover());
                let rect = egui::Rect::from_min_size(
                    egui::pos2(row.center().x - size.x / 2.0, row.min.y),
                    size,
                );
                if rect.intersects(clip) && first_visible.is_none() {
                    first_visible = Some(index);
                }
                if rect.intersects(preload) {
                    wanted.push(index);
                }
                if rect.intersects(clip) {
                    match &self.images[index] {
                        ImageSlot::Ready(texture) => {
                            ui.painter().image(
                                texture.id(),
                                rect,
                                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                        }
                        ImageSlot::Failed(message) => {
                            let button = egui::Rect::from_center_size(
                                rect.center(),
                                egui::vec2(size.x.min(240.0), 40.0),
                            );
                            if ui
                                .put(
                                    button,
                                    egui::Button::new(format!("Retry page {}", index + 1)),
                                )
                                .on_hover_text(message.as_str())
                                .clicked()
                            {
                                retry = Some(index);
                            }
                        }
                        _ => {
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                format!("Loading page {}…", index + 1),
                                egui::FontId::proportional(13.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                    }
                }
                if mode != ReadingMode::Webtoon {
                    ui.add_space(PAGE_GAP);
                }
            }
        });
        self.reader_row_width = row_width;
        self.current_page = first_visible.unwrap_or(self.current_page);
        self.book_cover_visible &= self.current_page == 0;
        self.fit_window_to_page(ctx, viewport, row_width, mode, scale);
        for index in wanted {
            self.load_image(index, ctx);
        }
        if let Some(index) = retry {
            self.images[index] = ImageSlot::Empty;
            self.load_image(index, ctx);
        }
    }

    /// `Ch. 47 · title`, falling back to whichever half the source supplied.
    fn chapter_title(&self) -> String {
        let Some(chapter) = self.chapter.as_ref() else {
            return String::new();
        };
        match chapter.chapter_number {
            Some(number) if chapter.name.trim().is_empty() => format!("Chapter {number}"),
            Some(number) => format!("Ch. {number} · {}", chapter.name),
            None => chapter.name.clone(),
        }
    }

    /// Older and newer releases in the deduplicated reading order.
    fn chapter_neighbours(&self) -> (bool, bool) {
        let Some(current) = self.chapter.as_ref() else {
            return (false, false);
        };
        let order = local::ordered_indices(&self.chapters, self.hide_duplicates);
        let Some(position) = order
            .iter()
            .position(|&index| self.chapters[index].url == current.url)
        else {
            return (false, false);
        };
        (position + 1 < order.len(), position > 0)
    }

    fn page_size_at(
        &self,
        index: usize,
        row_width: f32,
        viewport: egui::Vec2,
        mode: ReadingMode,
        scale: f32,
    ) -> egui::Vec2 {
        let aspect = match self.images.get(index) {
            Some(ImageSlot::Ready(texture)) => texture.size_vec2(),
            _ => egui::vec2(1.0, SLOT_RATIO),
        };
        page_size(aspect, egui::vec2(row_width, viewport.y), mode, scale)
    }

    /// Content offset of a page under the current layout, used to stay in place
    /// when the reading mode or size changes.
    fn page_offset(
        &self,
        page: usize,
        row_width: f32,
        viewport: egui::Vec2,
        mode: ReadingMode,
        scale: f32,
    ) -> f32 {
        let mut offset = 0.0;
        for index in 0..page.min(self.pages.len()) {
            offset += self.page_size_at(index, row_width, viewport, mode, scale).y;
            if mode != ReadingMode::Webtoon {
                offset += PAGE_GAP;
            }
        }
        offset
    }

    /// Shrink the window to the page so a fitted page has no empty bars beside
    /// or under it. Only ever narrows or shortens; the size from before the
    /// first shrink is kept so leaving the reader brings the browsing layout
    /// back.
    fn fit_window_to_page(
        &mut self,
        ctx: &egui::Context,
        viewport: egui::Vec2,
        row_width: f32,
        mode: ReadingMode,
        scale: f32,
    ) {
        let ready = matches!(
            self.images.get(self.current_page),
            Some(ImageSlot::Ready(_))
        );
        let page = self.page_size_at(self.current_page, row_width, viewport, mode, scale);
        let screen = ctx.screen_rect();
        // "Fit page" already uses the full height, and a smaller size there asks
        // for space around the page, so only a wider view has a band under it.
        let height_fit = self.reader_settings.fit_window_height
            && (mode != ReadingMode::FitPage || scale >= 1.0);
        // Measuring while the toolbar slides in or out would read a page that
        // has not settled, and an undecoded page has no aspect ratio yet.
        let settled = ready && !self.reader_bar_visible && self.reader_bar_slide <= 0.0;

        let width_target = if self.reader_settings.fit_window && settled {
            fitted_axis(row_width - page.x, screen.width(), MIN_WINDOW_WIDTH)
        } else {
            None
        };
        let height_target = if height_fit && settled {
            // `page_size` keeps PAGE_MARGIN free under the page; only the space
            // past it is an empty band.
            fitted_axis(
                viewport.y - PAGE_MARGIN - page.y,
                screen.height(),
                MIN_WINDOW_HEIGHT,
            )
        } else {
            None
        };

        let mut wanted = screen.size();
        let mut resize = false;
        if let Some(width) = width_target {
            wanted.x = width;
            resize = true;
        } else if !self.reader_settings.fit_window && std::mem::take(&mut self.reader_shrunk_width)
        {
            wanted.x = self.window.width;
            resize = true;
        }
        if let Some(height) = height_target {
            wanted.y = height;
            resize = true;
        } else if !height_fit && std::mem::take(&mut self.reader_shrunk_height) {
            wanted.y = self.window.height;
            resize = true;
        }
        if !resize {
            return;
        }
        self.reader_shrunk_width |= wanted.x < screen.width();
        self.reader_shrunk_height |= wanted.y < screen.height();
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(wanted));
    }

    /// Remember the size the user left the window at. A size the reader chose
    /// for itself is not the user's, so it is never recorded.
    fn remember_window_size(&mut self, ctx: &egui::Context) {
        // A borderless window has a taller content area than a decorated one, so
        // its size is not the size the user chose for the browsing screens.
        if self.borderless || self.reader_shrunk_width || self.reader_shrunk_height {
            return;
        }
        let size = ctx.screen_rect().size();
        let saved = egui::vec2(self.window.width, self.window.height);
        if (size - saved).length() < 1.0 {
            self.window_observed = None;
            return;
        }
        // A drag reports a new size on every pointer move; write once the size
        // has stayed put for a whole frame.
        let settled = self
            .window_observed
            .is_some_and(|seen| (seen - size).length() < 0.5);
        self.window_observed = Some(size);
        if !settled {
            return;
        }
        self.window = WindowState {
            width: size.x,
            height: size.y,
        };
        if let Err(error) = local::write(&self.data_root.join("window.json"), &self.window) {
            self.error = Some(format!("Could not save the window size: {error}"));
        }
    }

    fn restore_window(&mut self, ctx: &egui::Context) {
        let width = std::mem::take(&mut self.reader_shrunk_width);
        let height = std::mem::take(&mut self.reader_shrunk_height);
        if !width && !height {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            self.window.width,
            self.window.height,
        )));
    }
}

/// One compact, fully clickable chapter row: number, title, scanlator.
fn chapter_row(ui: &mut egui::Ui, chapter: &Chapter) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &chapter.name));
    if hovered || pressed {
        ui.painter().rect_filled(
            rect,
            5.0,
            if pressed {
                egui::Color32::from_gray(56)
            } else {
                HOVER
            },
        );
    }
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let number = chapter
        .chapter_number
        .map(|number| format!("{number}"))
        .unwrap_or_else(|| "—".to_owned());
    row.add_sized(
        [42.0, ROW_HEIGHT],
        egui::Label::new(egui::RichText::new(number).size(12.0).color(if hovered {
            egui::Color32::from_gray(198)
        } else {
            MUTED
        }))
        .halign(egui::Align::RIGHT),
    );
    let group = chapter.scanlator.as_deref().unwrap_or("").trim();
    let group_width = if group.is_empty() { 0.0 } else { 116.0 };
    let title_width = (row.available_width() - group_width - 12.0).max(60.0);
    row.add_sized(
        [title_width, ROW_HEIGHT],
        egui::Label::new(
            egui::RichText::new(&chapter.name)
                .size(13.0)
                .color(if hovered {
                    egui::Color32::from_gray(246)
                } else {
                    egui::Color32::from_gray(216)
                }),
        )
        .truncate(),
    );
    if !group.is_empty() {
        let width = row.available_width().min(group_width);
        row.add_sized(
            [width, ROW_HEIGHT],
            egui::Label::new(egui::RichText::new(group).size(11.0).color(if hovered {
                egui::Color32::from_gray(170)
            } else {
                egui::Color32::from_gray(132)
            }))
            .truncate()
            .halign(egui::Align::RIGHT),
        );
    }
    response
}

/// Cover captions get two left-aligned lines and an ellipsis, never a third.
fn two_line_title(text: &str, width: f32, color: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(13.0),
        color,
        width,
    );
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    job
}
/// Drop results from a series or chapter the user has already left.
fn current_event(event: &Event, generation: u64, series_job: u64, view_job: u64) -> bool {
    match event {
        Event::Cover { .. }
        | Event::PreparedChapter(..)
        | Event::DownloadProgress(..)
        | Event::DownloadFinished(_) => true,
        Event::Details(_) | Event::Chapters(_) | Event::ChapterRefreshDone(_) => {
            generation == series_job
        }
        _ => generation == view_job,
    }
}

fn page(result: rustga::Result<MangaPage>) -> Outcome<MangaPage> {
    result.map_err(|error| error.to_string())
}

fn image_url(page: &Page) -> &str {
    page.image_url.as_deref().unwrap_or(&page.url)
}

fn fetch_page(
    http: &rustga::HttpSource,
    cache: &Path,
    url: &str,
    cancel: &AtomicBool,
    width: u32,
) -> (Outcome<DecodedPage>, ImageTiming) {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    if cancel.load(Ordering::Relaxed) {
        return (
            Err("Page load canceled".into()),
            (false, Duration::ZERO, Duration::ZERO),
        );
    }
    if let Some(root) = cache.parent() {
        if downloads::page_path(root, url).is_file() {
            let result = downloads::prepare_page(root, url).map(|page| DecodedPage {
                width: page.width,
                height: page.height,
                pixels: page.pixels,
            });
            return (result, (true, started.elapsed(), Duration::ZERO));
        }
    }
    let path = cache.join(format!("{:016x}.page", local::url_key(url)));
    if let Ok(bytes) = std::fs::read(&path) {
        let fetch = started.elapsed();
        let decoding = Instant::now();
        if !cancel.load(Ordering::Relaxed) {
            if let Ok(page) = decode(&bytes, width) {
                return (Ok(page), (true, fetch, decoding.elapsed()));
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return (
            Err("Page load canceled".to_owned()),
            (false, started.elapsed(), Duration::ZERO),
        );
    }
    let bytes = http.get_bytes(url).map_err(|error| error.to_string());
    let fetch = started.elapsed();
    let decoding = Instant::now();
    let page = bytes.and_then(|bytes| {
        if cancel.load(Ordering::Relaxed) {
            return Err("Page load canceled".to_owned());
        }
        let page = decode(&bytes, width)?;
        Ok((page, bytes))
    });
    let timing = (false, fetch, decoding.elapsed());
    (
        page.map(|(page, bytes)| {
            let _ = local::cache_page(&path, &bytes);
            page
        }),
        timing,
    )
}

fn page_texture(ctx: &egui::Context, index: usize, page: DecodedPage) -> egui::TextureHandle {
    ctx.load_texture(
        format!("page-{index}"),
        egui::ColorImage::from_rgba_unmultiplied([page.width, page.height], &page.pixels),
        egui::TextureOptions::LINEAR,
    )
}

fn decode(bytes: &[u8], width: u32) -> Outcome<DecodedPage> {
    decode_sized(bytes, width, (4_000_000 / width.max(1)).min(20_000))
}

fn decode_sized(bytes: &[u8], width: u32, height: u32) -> Outcome<DecodedPage> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let rgba = if image.width() <= width && image.height() <= height {
        image.into_rgba8()
    } else {
        image.thumbnail(width, height).into_rgba8()
    };
    let (width, height) = rgba.dimensions();
    Ok(DecodedPage {
        width: width as usize,
        height: height as usize,
        pixels: rgba.into_raw(),
    })
}

fn page_size(image: egui::Vec2, viewport: egui::Vec2, mode: ReadingMode, scale: f32) -> egui::Vec2 {
    let width = viewport.x.max(1.0);
    let height = (viewport.y - PAGE_MARGIN).max(1.0);
    let ratio = image.y / image.x.max(1.0);
    let fitted_width = match mode {
        ReadingMode::FitPage | ReadingMode::MangaBook => width.min(height / ratio),
        ReadingMode::FitWidth => width,
        ReadingMode::Webtoon => width.min(760.0),
    } * scale;
    egui::vec2(fitted_width, fitted_width * ratio)
}

/// Window span along one axis once the empty bar past the page is removed, or
/// `None` when the bar is too thin to be worth a resize or the window would drop
/// below `min`.
fn fitted_axis(bars: f32, current: f32, min: f32) -> Option<f32> {
    let target = (current - bars).max(min);
    (bars > SIDE_BAR_MIN && current - target > SIDE_BAR_MIN).then_some(target)
}

/// SF Pro for the interface, plus a broad fallback for the Chinese, Japanese and
/// Korean names MangaDot returns. SF Pro carries no CJK glyphs at all, so without
/// the fallback those characters are drawn as empty boxes.
#[cfg(target_os = "macos")]
fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut installed = false;

    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/SFNS.ttf") {
        fonts.font_data.insert(
            "SF Pro".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "SF Pro".to_owned());
        installed = true;
    }

    // Arial Unicode is TrueType, which the rasteriser handles, and covers CJK
    // ideographs, kana and Hangul. AppleGothic is the narrower stand-in.
    for (name, path) in [
        (
            "Arial Unicode",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ),
        (
            "AppleGothic",
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
        ),
    ] {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        let family = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        // Right after the interface font, before egui's Latin-only defaults.
        let index = family.len().min(1);
        family.insert(index, name.to_owned());
        installed = true;
        break;
    }

    if installed {
        ctx.set_fonts(fonts);
    }
}

#[cfg(not(target_os = "macos"))]
fn configure_fonts(_ctx: &egui::Context) {}

fn configure_style(ctx: &egui::Context) {
    configure_fonts(ctx);

    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = egui::Color32::from_gray(18);
    style.visuals.override_text_color = Some(egui::Color32::from_gray(228));
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.selection.bg_fill = egui::Color32::from_gray(56);
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(232));
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.faint_bg_color = egui::Color32::from_gray(26);
    style.visuals.window_stroke = egui::Stroke::new(1.0_f32, SEPARATOR);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(40);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(36);
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(52);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(48);
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    style.visuals.widgets.active.bg_fill = egui::Color32::from_gray(62);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_gray(58);
    style.visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, SEPARATOR);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 28.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(11.5));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
    // Chapter rows and cover captions are click targets. A selectable label is a
    // click-and-drag widget: it takes the click and the hover from the row below it,
    // so the row never opens. Nothing in this UI needs text selection.
    style.interaction.selectable_labels = false;
    ctx.set_style(style);
}
/// Reshape linear animation progress so the bar accelerates as it moves instead of
/// travelling at a constant rate; the finish is `e^4` times faster than the start.
fn ease_in_exponential(progress: f32) -> f32 {
    const RATE: f32 = 4.0;
    let progress = progress.clamp(0.0, 1.0);
    ((RATE * progress).exp() - 1.0) / (RATE.exp() - 1.0)
}

/// The other way round: most of the way at once, then settling. A cover should
/// react the moment the pointer arrives, so its growth uses this one.
fn ease_out_exponential(progress: f32) -> f32 {
    1.0 - (-10.0 * progress.clamp(0.0, 1.0)).exp2()
}

/// The real macOS window buttons live in the title bar. The reader makes that
/// bar transparent and lets the content fill the window, so the buttons float
/// over the page; the bar is faded back in with the reader's own bar.
#[cfg(target_os = "macos")]
mod titlebar {
    use objc2_app_kit::{
        NSView, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// Run `f` with the AppKit window behind the egui view. `update` runs on the
    /// main thread, which is where AppKit may be used.
    fn with_window(frame: &eframe::Frame, f: impl FnOnce(&NSWindow)) {
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return;
        };
        let view = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
        if let Some(window) = view.window() {
            f(&window);
        }
    }

    /// A titled window whose title bar draws no background, shows no title and
    /// covers no content, while its buttons stay exactly where macOS put them.
    pub fn set_reader_mode(frame: &eframe::Frame, reader: bool) {
        with_window(frame, |window| {
            window.setTitlebarAppearsTransparent(reader);
            window.setTitleVisibility(if reader {
                NSWindowTitleVisibility::Hidden
            } else {
                NSWindowTitleVisibility::Visible
            });
            let mut mask = window.styleMask();
            if reader {
                mask |= NSWindowStyleMask::FullSizeContentView;
            } else {
                mask &= !NSWindowStyleMask::FullSizeContentView;
            }
            window.setStyleMask(mask);
            show_buttons(window, if reader { 0.0 } else { 1.0 });
        });
    }

    /// Fade the window buttons with the reader bar. A hidden button takes no
    /// clicks, so the bar cannot close the window while it is away.
    pub fn fade_buttons(frame: &eframe::Frame, alpha: f32) {
        with_window(frame, |window| show_buttons(window, alpha));
    }

    fn show_buttons(window: &NSWindow, alpha: f32) {
        for kind in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(button) = window.standardWindowButton(kind) {
                button.setAlphaValue(alpha as f64);
                button.setHidden(alpha <= 0.0);
            }
        }
    }
}

fn set_reader_chrome(frame: &mut eframe::Frame, reader: bool) {
    #[cfg(target_os = "macos")]
    titlebar::set_reader_mode(frame, reader);
    #[cfg(not(target_os = "macos"))]
    let _ = (frame, reader);
}

fn fade_reader_buttons(frame: &eframe::Frame, alpha: f32) {
    #[cfg(target_os = "macos")]
    titlebar::fade_buttons(frame, alpha);
    #[cfg(not(target_os = "macos"))]
    let _ = (frame, alpha);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain(ctx);
        self.remember_window_size(ctx);

        // The reader keeps the real window buttons but drops the title bar around
        // them; every other screen keeps the normal window chrome.
        let borderless = cfg!(target_os = "macos") && self.screen == Screen::Reader;
        if borderless != self.borderless {
            self.borderless = borderless;
            set_reader_chrome(frame, borderless);
            if !borderless {
                // The restored title bar takes its height out of the content, so
                // ask again for the size the user chose. Must follow Decorations.
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    self.window.width,
                    self.window.height,
                )));
            }
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.back(ctx);
        }

        match self.screen {
            Screen::Reader => {
                // The bar stays out of the way while reading. The bottom edge of
                // the window, and any open popup from it, brings it back up.
                let reveal = if self.reader_bar_visible {
                    READER_BAR_REVEAL * READER_BAR_REVEAL_GROWTH
                } else {
                    READER_BAR_REVEAL
                };
                let screen_bottom = ctx.screen_rect().bottom();
                let near_bar = ctx
                    .pointer_hover_pos()
                    .is_some_and(|pointer| pointer.y >= screen_bottom - reveal);
                // A quick pass over the bottom edge should not summon the bar; it
                // rises only once the pointer has rested there a moment.
                let now = ctx.input(|input| input.time);
                let mut dwell = 0.0;
                if near_bar {
                    dwell = now - *self.reader_bar_reveal_since.get_or_insert(now);
                } else {
                    self.reader_bar_reveal_since = None;
                }
                self.reader_bar_visible = egui::Popup::is_any_open(ctx)
                    || dwell >= READER_BAR_REVEAL_DELAY
                    || (self.reader_bar_visible && near_bar);
                if near_bar && !self.reader_bar_visible {
                    // Nothing else moves the pointer while it waits, so ask for
                    // the frame that lands the moment the delay runs out.
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                        READER_BAR_REVEAL_DELAY - dwell,
                    ));
                }
                let progress = ctx.animate_bool_with_time(
                    egui::Id::new("reader-bar"),
                    self.reader_bar_visible,
                    0.2,
                );
                let slide = ease_in_exponential(progress);
                self.reader_bar_slide = slide;
                fade_reader_buttons(frame, slide);
                if slide > 0.0 {
                    // A bar of its own, floating over the page: it rises from the
                    // bottom edge and fades as it travels, and it is the only place
                    // the window buttons live while the reader is open.
                    egui::Area::new(egui::Id::new("reader_titlebar"))
                        .order(egui::Order::Foreground)
                        .constrain(false)
                        .fixed_pos(egui::pos2(0.0, screen_bottom - slide * READER_BAR_HEIGHT))
                        .show(ctx, |ui| {
                            ui.set_opacity(slide);
                            let width = ctx.screen_rect().width();
                            let (rect, drag) = ui.allocate_exact_size(
                                egui::vec2(width, READER_BAR_HEIGHT),
                                egui::Sense::click_and_drag(),
                            );
                            ui.painter().rect_filled(rect, 0.0, READER_BAR_FILL);
                            // The title bar itself draws no background, so the
                            // bar is what moves the window.
                            if drag.drag_started() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(rect.shrink2(egui::vec2(14.0, 6.0))),
                                |ui| self.reader_bar(ui, ctx),
                            );
                        });
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| self.reader(ui, ctx));
                self.save_position();
                self.warm_next_chapter(ctx);
            }
            Screen::Browse => {
                egui::TopBottomPanel::top("header")
                    .frame(
                        egui::Frame::new()
                            .fill(BACKGROUND)
                            .inner_margin(egui::Margin::symmetric(20, 8)),
                    )
                    .show(ctx, |ui| self.header(ui, ctx));
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(BACKGROUND)
                            .inner_margin(egui::Margin::symmetric(20, 10)),
                    )
                    .show(ctx, |ui| self.browse(ui, ctx));
            }
            Screen::Series => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(BACKGROUND)
                            .inner_margin(egui::Margin::symmetric(20, 12)),
                    )
                    .show(ctx, |ui| self.series_view(ui, ctx));
            }
        }
    }
}
/// Without a terminal, a panic or a stopped event loop leaves only a vanished
/// window. Keep the reason beside the other local files so it can be reported.
fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed thread");
        let report = format!(
            "{name} panicked: {info}\n\n{}\n",
            std::backtrace::Backtrace::force_capture()
        );
        write_crash_log(&report);
        eprintln!("rustga panicked; details written to crash.log");
    }));
}

fn write_crash_log(report: &str) {
    let directory = local::directory();
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join("crash.log"), report);
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "macos")]
    if std::env::args().nth(1).as_deref() == Some("--verify-mangaball") {
        verification::run();
        return Ok(());
    }
    install_crash_log();
    let sources = rustga::extensions::builtin()
        .into_iter()
        .map(Arc::<dyn Source>::from)
        .collect::<Vec<_>>();
    let source = sources
        .first()
        .cloned()
        .expect("at least one built-in source is required");

    let window = WindowState::load(&local::directory());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([window.width, window.height])
            .with_min_inner_size([MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT])
            .with_clamp_size_to_monitor_size(true),
        ..Default::default()
    };

    let result = eframe::run_native(
        "rustga",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new_with_sources(
                source,
                sources,
                &cc.egui_ctx,
                window,
            )))
        }),
    );
    if let Err(error) = &result {
        write_crash_log(&format!("the window stopped: {error}\n"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_pages_stay_inside_the_viewport_and_webtoons_keep_their_ratio() {
        let viewport = egui::vec2(1100.0, 660.0);
        for image in [
            egui::vec2(800.0, 1200.0),
            egui::vec2(1600.0, 900.0),
            egui::vec2(800.0, 8000.0),
        ] {
            let fitted = page_size(image, viewport, ReadingMode::FitPage, 1.0);
            assert!(fitted.x <= viewport.x && fitted.y <= viewport.y);
            assert!((fitted.x / fitted.y - image.x / image.y).abs() < 0.001);
            let strip = page_size(image, viewport, ReadingMode::Webtoon, 0.8);
            assert_eq!(strip.x, 608.0);
            assert!((strip.y / strip.x - image.y / image.x).abs() < 0.001);
        }
    }

    #[test]
    fn ignores_results_from_a_view_the_user_left() {
        let stale = Event::Pages(Ok(Vec::new()));
        assert!(current_event(&stale, 4, 1, 4));
        assert!(!current_event(&stale, 3, 1, 4));

        let chapters = Event::Chapters(ChapterList::default());
        assert!(current_event(&chapters, 2, 2, 9));
        assert!(!current_event(&chapters, 1, 2, 9));
    }

    /// MangaDot returns Chinese, Japanese and Korean names. The font stack must
    /// resolve them; SF Pro alone draws each one as an empty box.
    #[test]
    #[cfg(target_os = "macos")]
    fn names_in_cjk_scripts_have_glyphs() {
        if !std::path::Path::new("/System/Library/Fonts/Supplemental/Arial Unicode.ttf").exists() {
            return; // Optional system font absent; there is nothing to check.
        }
        let ctx = egui::Context::default();
        configure_fonts(&ctx);
        let font = egui::FontId::proportional(14.0);
        // Fonts are only built at the start of a pass, so drive one frame.
        let mut checked: Vec<(&str, bool)> = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            checked = ["饼饼不画饼", "古拉", "あア", "한"]
                .iter()
                .map(|name| (*name, ctx.fonts(|fonts| fonts.has_glyphs(&font, name))))
                .collect();
        });
        for (name, has_glyphs) in checked {
            assert!(has_glyphs, "{name} should render");
        }
    }

    #[test]
    fn older_reader_settings_turn_the_page_fit_on_once() {
        let mut old: ReaderSettings =
            serde_json::from_str(r#"{"mode":"Webtoon","scale":0.76,"fit_window":false}"#).unwrap();
        assert!(old.upgrade());
        assert!(old.fit_window);
        assert!(old.fit_window_height);
        assert_eq!(old.version, READER_SETTINGS_VERSION);
        assert_eq!(old.scale, 0.76);
        assert!(matches!(old.mode, ReadingMode::Webtoon));

        // A current file records the user's choice, including an opt-out.
        let mut opted_out: ReaderSettings = serde_json::from_str(
            r#"{"mode":"FitPage","scale":1.0,"fit_window":false,"version":1}"#,
        )
        .unwrap();
        assert!(!opted_out.upgrade());
        assert!(!opted_out.fit_window);
    }

    /// In "Fit width" a page shorter than the window leaves a band under it;
    /// taking that band must not turn into growing the window or shrinking it
    /// away in steps.
    #[test]
    fn a_page_shorter_than_the_window_shrinks_the_height_and_never_grows_it() {
        // 150 points of empty band: the window loses exactly those.
        assert_eq!(fitted_axis(150.0, 800.0, MIN_WINDOW_HEIGHT), Some(650.0));
        // A page taller than the view is no reason to grow the window.
        assert_eq!(fitted_axis(-40.0, 800.0, MIN_WINDOW_HEIGHT), None);
        // A band thinner than the resize threshold is left alone.
        assert_eq!(fitted_axis(SIDE_BAR_MIN, 800.0, MIN_WINDOW_HEIGHT), None);
        assert_eq!(fitted_axis(0.0, 800.0, MIN_WINDOW_HEIGHT), None);
        // The minimum window height still wins over the page.
        assert_eq!(
            fitted_axis(650.0, 700.0, MIN_WINDOW_HEIGHT),
            Some(MIN_WINDOW_HEIGHT)
        );
    }

    #[test]
    fn unusable_window_sizes_fall_back_and_small_ones_are_lifted() {
        let broken = WindowState {
            width: f32::NAN,
            height: 12.0,
        }
        .sanitized();
        assert_eq!(broken.width, DEFAULT_WINDOW_WIDTH);
        assert_eq!(broken.height, MIN_WINDOW_HEIGHT);
        let small = WindowState {
            width: 200.0,
            height: 300.0,
        }
        .sanitized();
        assert_eq!(small.width, MIN_WINDOW_WIDTH);
        assert_eq!(small.height, MIN_WINDOW_HEIGHT);
        let kept = WindowState {
            width: 990.0,
            height: 720.0,
        }
        .sanitized();
        assert_eq!((kept.width, kept.height), (990.0, 720.0));
    }

    /// A source with canned answers, so the state machine can be driven by hand.
    struct TestSource {
        info: rustga::source::SourceInfo,
        pages: Vec<Page>,
        snapshots: Vec<ChapterList>,
    }

    impl TestSource {
        fn new(pages: Vec<Page>, snapshots: Vec<ChapterList>) -> Self {
            Self {
                info: rustga::source::SourceInfo::new("Test", "en", 1, "https://example.com"),
                pages,
                snapshots,
            }
        }
    }

    impl Source for TestSource {
        fn info(&self) -> &rustga::source::SourceInfo {
            &self.info
        }

        fn popular(&self, _page: i64) -> rustga::Result<MangaPage> {
            Ok(MangaPage::single(Vec::new()))
        }

        fn details(&self, manga: &Manga) -> rustga::Result<Manga> {
            Ok(manga.clone())
        }

        fn chapters(&self, _manga: &Manga) -> rustga::Result<Vec<Chapter>> {
            Ok(self
                .snapshots
                .last()
                .map_or_else(Vec::new, |list| list.chapters.clone()))
        }

        fn refresh_chapters(
            &self,
            _manga: &Manga,
            _cached: ChapterList,
            publish: &mut dyn FnMut(&ChapterList) -> bool,
        ) -> rustga::Result<()> {
            for snapshot in &self.snapshots {
                if !publish(snapshot) {
                    break;
                }
            }
            Ok(())
        }

        fn pages(&self, _chapter: &Chapter) -> rustga::Result<Vec<Page>> {
            Ok(self.pages.clone())
        }
    }

    #[test]
    fn a_prepared_download_opens_with_its_texture_already_ready() {
        let source = Arc::new(TestSource::new(Vec::new(), Vec::new()));
        let root = PathBuf::from(format!(
            "target/cache-test-open-ready-{}",
            std::process::id()
        ));
        let mut app = test_app(source, root.clone());
        let ctx = egui::Context::default();
        let texture = page_texture(
            &ctx,
            0,
            DecodedPage {
                width: 2,
                height: 2,
                pixels: vec![255; 16],
            },
        );
        let page = Page {
            index: 0,
            url: "https://example.com/page.png".into(),
            image_url: None,
        };
        let chapter = Chapter {
            url: "https://example.com/chapter".into(),
            ..Default::default()
        };
        app.remember_texture(page.url.clone(), texture.clone());
        app.prepared_manifests
            .insert(chapter.url.clone(), vec![page]);
        app.open_chapter(&ctx, chapter.clone());
        assert!(!app.busy);
        assert!(matches!(&app.images[0], ImageSlot::Ready(ready) if ready.id() == texture.id()));
        app.open_chapter(&ctx, chapter);
        assert!(!app.busy);
        assert!(matches!(&app.images[0], ImageSlot::Ready(ready) if ready.id() == texture.id()));
        if root.exists() {
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// An `App` without the window or the opening listing load, so the state
    /// machine can be stepped directly.
    fn test_app(source: Arc<dyn Source>, data_root: PathBuf) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            source_options: vec![source.clone()],
            source,
            tx,
            rx,
            series_job: 0,
            view_job: 0,
            refreshing: false,
            complete: false,
            chapter_status: None,
            cancel_refresh: Arc::new(AtomicBool::new(false)),
            cancel_images: Arc::new(AtomicBool::new(false)),
            image_http: None,
            image_in_flight: 0,
            hide_duplicates: true,
            covers: HashMap::new(),
            covers_in_flight: 0,
            reader_settings: ReaderSettings::default(),
            data_root,
            current_page: 0,
            book_turn: None,
            book_drag: None,
            saved_position: None,
            warm_chapter: None,
            warming: None,
            book_pending_turn: None,
            book_swipe: book::Swipe::default(),
            book_cover_visible: true,
            anchor_page: None,
            reader_row_width: 1.0,
            reader_bar_visible: false,
            reader_bar_slide: 0.0,
            reader_bar_reveal_since: None,
            borderless: false,
            reader_shrunk_width: false,
            reader_shrunk_height: false,
            window: WindowState::default(),
            window_observed: None,
            search_results: false,
            screen: Screen::Browse,
            tab: Tab::Popular,
            query: String::new(),
            status: String::new(),
            error: None,
            busy: false,
            mangas: Vec::new(),
            series: None,
            chapters: Vec::new(),
            chapter: None,
            pages: Vec::new(),
            images: Vec::new(),
            page_dimensions: Vec::new(),
            download_cancel: None,
            download_status: String::new(),
            downloaded: std::collections::HashSet::new(),
            ready_pages: HashMap::new(),
            ready_clock: 0,
            prepared_manifests: HashMap::new(),
            preparing_chapter: None,
        }
    }

    /// The pages of a chapter the user has left are dropped by the job filter,
    /// so nothing but `back` can lower the busy flag the load raised.
    #[test]
    fn leaving_the_reader_clears_the_pending_page_load() {
        let ctx = egui::Context::default();
        let pages = (0..3)
            .map(|index| Page {
                index,
                url: format!("https://example.com/{index}.jpg"),
                image_url: None,
            })
            .collect();
        let mut app = test_app(
            Arc::new(TestSource::new(pages, Vec::new())),
            PathBuf::from(format!("target/cache-test-{}-busy", std::process::id())),
        );
        let chapter = Chapter {
            url: "/chapter/1".to_owned(),
            name: "Chapter 1".to_owned(),
            chapter_number: Some(1.0),
            ..Default::default()
        };

        app.open_chapter(&ctx, chapter);
        assert!(app.busy);

        // Leaving before the pages arrive cancels the load; its event is stale
        // from now on and will never be handled.
        app.back(&ctx);
        assert!(
            !app.busy,
            "a cancelled page load must not leave the UI busy"
        );

        std::thread::sleep(std::time::Duration::from_millis(100));
        app.drain(&ctx);
        assert!(app.pages.is_empty());
        assert!(!app.busy);
    }

    /// A refresh that republishes the same number of chapters with different
    /// ones must still reach the cache, or replaced chapters survive on disk.
    #[test]
    fn a_changed_chapter_snapshot_is_saved_even_when_its_size_is_unchanged() {
        let ctx = egui::Context::default();
        let chapter = |url: &str| Chapter {
            url: url.to_owned(),
            name: url.to_owned(),
            ..Default::default()
        };
        let source = TestSource::new(
            Vec::new(),
            vec![
                ChapterList {
                    chapters: vec![chapter("a"), chapter("b")],
                    complete: true,
                },
                ChapterList {
                    chapters: vec![chapter("a"), chapter("c")],
                    complete: true,
                },
            ],
        );
        let root = PathBuf::from(format!("target/cache-test-{}-snapshot", std::process::id()));
        let path = local::chapter_path(&root, source.info().id, "https://example.com/series/1");
        let mut app = test_app(Arc::new(source), root.clone());
        app.open_series(&ctx, Manga::new("/series/1", "Test"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saved = None;
        while std::time::Instant::now() < deadline {
            app.drain(&ctx);
            if let Ok(Some(list)) = local::read::<ChapterList>(&path) {
                saved = Some(list);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let saved = saved.expect("the refresh should have written the chapter cache");
        assert!(saved.complete);
        assert_eq!(saved.chapters, vec![chapter("a"), chapter("c")]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&root);
    }
}
