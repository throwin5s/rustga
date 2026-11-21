//! rustga — a minimal desktop reader.
//!
//! Cover browsing, series details and a fitted-page / webtoon reader.
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
use rustga::{local, source::ChapterList, Source};

/// Width every page is scaled to before it becomes a texture; the originals are
/// far larger than any screen and would exhaust GPU memory across a chapter.
const PAGE_WIDTH: u32 = 800;

/// Pages kept ready on either side of the viewport.
const PREFETCH_PAGES: usize = 2;

/// Decode at most this many images at once; each is a thread.
const MAX_CONCURRENT_IMAGES: usize = 4;

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
/// Height of the reader toolbar, including its vertical padding.
const READER_BAR_HEIGHT: f32 = 40.0;
/// A pointer within this many points of the top edge brings the toolbar back.
const READER_BAR_REVEAL: f32 = 56.0;
/// How long the pointer has to rest near the top edge first. A quick pass on the
/// way to a page should not bring the toolbar down; only a deliberate reach does.
const READER_BAR_REVEAL_DELAY: f64 = 0.3;
/// Once the toolbar is down, the reveal band grows by this much so it does not
/// snap away while the pointer is still near it.
const READER_BAR_REVEAL_GROWTH: f32 = 1.5;
/// The reader can shrink the window to the page; the rest of the UI still needs
/// this much width to lay out.
const MIN_WINDOW_WIDTH: f32 = 520.0;
/// Side space narrower than this is not worth a window resize.
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
}

impl ReadingMode {
    fn label(self) -> &'static str {
        match self {
            Self::FitPage => "Fit page",
            Self::FitWidth => "Fit width",
            Self::Webtoon => "Webtoon",
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
    },
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
}

struct App {
    source: Arc<dyn Source>,
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
    anchor_page: Option<usize>,
    reader_row_width: f32,
    reader_bar_visible: bool,
    reader_bar_slide: f32,
    /// When the pointer entered the toolbar's reveal band, so only a pointer
    /// that rests there brings the bar down.
    reader_bar_reveal_since: Option<f64>,
    /// Window width from before the reader shrank it, restored on the way out.
    reader_window_width: Option<f32>,
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
}

impl App {
    fn new(source: Arc<dyn Source>, ctx: &egui::Context, window: WindowState) -> Self {
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
            anchor_page: None,
            reader_row_width: 1.0,
            reader_bar_visible: false,
            reader_bar_slide: 0.0,
            reader_bar_reveal_since: None,
            reader_window_width: None,
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
        };
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

    fn load_listing(&mut self, ctx: &egui::Context, tab: Tab) {
        self.screen = Screen::Browse;
        self.search_results = false;
        self.view_job += 1;
        self.tab = tab;
        self.busy = true;
        self.error = None;
        self.status = "Loading…".to_owned();
        self.spawn(ctx, self.view_job, move |source| match tab {
            Tab::Popular => Event::Popular(page(source.popular(1))),
            Tab::Latest => Event::Latest(page(source.latest(1))),
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
        self.current_page = 0;
        self.screen = Screen::Reader;
        self.chapter = Some(chapter.clone());
        self.pages.clear();
        self.images.clear();
        self.anchor_page = None;
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
        self.spawn(ctx, self.view_job, move |source| {
            Event::Pages(match source.pages(&chapter) {
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
        if !matches!(slot, ImageSlot::Empty) || self.image_in_flight >= MAX_CONCURRENT_IMAGES {
            return;
        }
        let Some(http) = self.image_http.clone() else {
            self.images[index] = ImageSlot::Failed("No image client".to_owned());
            return;
        };
        let url = self.pages[index].url.clone();
        let generation = self.view_job;
        let cancel = self.cancel_images.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();

        self.images[index] = ImageSlot::Loading;
        self.image_in_flight += 1;
        thread::spawn(move || {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let page = http
                .get_bytes(&url)
                .map_err(|error| error.to_string())
                .and_then(|bytes| decode(&bytes));
            let _ = tx.send((generation, Event::Image { index, page }));
            ctx.request_repaint();
        });
    }

    fn back(&mut self, ctx: &egui::Context) {
        match self.screen {
            Screen::Reader => {
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
                            self.pages = pages;
                            self.images = (0..self.pages.len()).map(|_| ImageSlot::Empty).collect();
                            self.load_image(0, ctx);
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Event::Image { index, page } => {
                    self.image_in_flight = self.image_in_flight.saturating_sub(1);
                    let Some(slot) = self.images.get_mut(index) else {
                        continue;
                    };
                    *slot = match page {
                        Ok(decoded) => {
                            let image = egui::ColorImage::from_rgba_unmultiplied(
                                [decoded.width, decoded.height],
                                &decoded.pixels,
                            );
                            let texture = ctx.load_texture(
                                format!("page-{index}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            );
                            ImageSlot::Ready(texture)
                        }
                        Err(error) => ImageSlot::Failed(error),
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
        if !ui.is_rect_visible(rect) || self.covers.contains_key(url) || self.covers_in_flight >= 4
        {
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
        thread::spawn(move || {
            let image = http
                .get_bytes(&url)
                .map_err(|e| e.to_string())
                .and_then(|bytes| decode_sized(&bytes, 360, 540));
            let _ = tx.send((0, Event::Cover { url, image }));
            ctx.request_repaint();
        });
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Rustga").size(15.0).strong());
            ui.add_space(14.0);
            for (tab, name) in [(Tab::Popular, "Popular"), (Tab::Latest, "Latest")] {
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
                ui.menu_button(egui::RichText::new(&source).size(12.0), |ui| {
                    ui.label(egui::RichText::new(&source).strong());
                    ui.label(egui::RichText::new("Only installed source").small().weak());
                });
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
                    if self.tab == Tab::Popular {
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
        let mut chosen = None;
        let mut go_back = false;

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
                                if chapter_row(ui, chapter).clicked() {
                                    chosen = Some(chapter.clone());
                                }
                            }
                        });
                },
            );
        });

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
            if ui
                .add(egui::Button::new(egui::RichText::new("Back").size(13.0)).frame(false))
                .clicked()
            {
                go_back = true;
            }
            ui.add_space(10.0);
            let title_width = (ui.available_width() - 360.0).max(90.0);
            ui.add_sized(
                [title_width, 24.0],
                egui::Label::new(egui::RichText::new(self.chapter_title()).size(13.0)).truncate(),
            );
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
                if !self.pages.is_empty() {
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
                        ui.horizontal(|ui| {
                            for mode in [
                                ReadingMode::FitPage,
                                ReadingMode::FitWidth,
                                ReadingMode::Webtoon,
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
                            settings_changed |= ui
                                .add_enabled(
                                    !self.reader_settings.fit_window,
                                    egui::Slider::new(&mut self.reader_settings.scale, 0.4..=1.0)
                                        .show_value(false),
                                )
                                .changed();
                            let shown_size = if self.reader_settings.fit_window {
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
                            .checkbox(
                                &mut self.reader_settings.fit_window,
                                egui::RichText::new("Fit window to page").size(12.0),
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
    /// it. Only ever narrows; the width from before the first shrink is kept so
    /// leaving the reader brings the browsing layout back.
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
        let side_bars = row_width - page.x;
        let screen = ctx.screen_rect();
        let width = (screen.width() - side_bars).max(MIN_WINDOW_WIDTH);
        if !self.reader_settings.fit_window {
            self.restore_window(ctx);
            return;
        }
        // Measuring while the toolbar slides in or out would read a page that
        // has not settled, and an undecoded page has no aspect ratio yet.
        if self.reader_bar_visible || self.reader_bar_slide > 0.0 || !ready {
            return;
        }
        if side_bars <= SIDE_BAR_MIN || screen.width() - width <= SIDE_BAR_MIN {
            return;
        }
        self.reader_window_width.get_or_insert(screen.width());
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            width,
            screen.height(),
        )));
    }

    /// Remember the size the user left the window at. A size the reader chose
    /// for itself is not the user's, so it is never recorded.
    fn remember_window_size(&mut self, ctx: &egui::Context) {
        if self.reader_window_width.is_some() {
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
        if self.reader_window_width.take().is_none() {
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
        Event::Cover { .. } => true,
        Event::Details(_) | Event::Chapters(_) | Event::ChapterRefreshDone(_) => {
            generation == series_job
        }
        _ => generation == view_job,
    }
}

fn page(result: rustga::Result<MangaPage>) -> Outcome<MangaPage> {
    result.map_err(|error| error.to_string())
}

fn decode(bytes: &[u8]) -> Outcome<DecodedPage> {
    decode_sized(bytes, PAGE_WIDTH, 20_000)
}

fn decode_sized(bytes: &[u8], width: u32, height: u32) -> Outcome<DecodedPage> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let thumbnail = image.thumbnail(width, height);
    let rgba = thumbnail.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedPage {
        width: width as usize,
        height: height as usize,
        pixels: rgba.into_raw(),
    })
}

fn page_size(image: egui::Vec2, viewport: egui::Vec2, mode: ReadingMode, scale: f32) -> egui::Vec2 {
    let width = viewport.x.max(1.0);
    let height = (viewport.y - 24.0).max(1.0);
    let ratio = image.y / image.x.max(1.0);
    let fitted_width = match mode {
        ReadingMode::FitPage => width.min(height / ratio),
        ReadingMode::FitWidth => width,
        ReadingMode::Webtoon => width.min(760.0),
    } * scale;
    egui::vec2(fitted_width, fitted_width * ratio)
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain(ctx);
        self.remember_window_size(ctx);

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.back(ctx);
        }

        match self.screen {
            Screen::Reader => {
                // The toolbar stays out of the way while reading. The top edge of
                // the window, and any open popup from it, brings it back down.
                let reveal = if self.reader_bar_visible {
                    READER_BAR_REVEAL * READER_BAR_REVEAL_GROWTH
                } else {
                    READER_BAR_REVEAL
                };
                let near_top = ctx
                    .pointer_hover_pos()
                    .is_some_and(|pointer| pointer.y <= reveal);
                // A quick pass over the top edge should not summon the bar; it
                // comes down only once the pointer has rested there a moment.
                let now = ctx.input(|input| input.time);
                let mut dwell = 0.0;
                if near_top {
                    dwell = now - *self.reader_bar_reveal_since.get_or_insert(now);
                } else {
                    self.reader_bar_reveal_since = None;
                }
                self.reader_bar_visible = egui::Popup::is_any_open(ctx)
                    || dwell >= READER_BAR_REVEAL_DELAY
                    || (self.reader_bar_visible && near_top);
                if near_top && !self.reader_bar_visible {
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
                if slide > 0.0 {
                    let height = READER_BAR_HEIGHT * slide;
                    egui::TopBottomPanel::top("reader_bar")
                        .min_height(height)
                        .max_height(height)
                        .frame(egui::Frame::new().fill(SURFACE))
                        .show(ctx, |ui| {
                            // Slide the content with the shrinking panel so the bar
                            // scrolls out of the top edge instead of being squeezed.
                            let origin = ui.min_rect().min;
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(
                                    origin.x + 14.0,
                                    origin.y + 6.0 - (1.0 - slide) * READER_BAR_HEIGHT,
                                ),
                                egui::vec2(
                                    (ui.available_width() - 28.0).max(1.0),
                                    READER_BAR_HEIGHT - 12.0,
                                ),
                            );
                            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                                self.reader_bar(ui, ctx)
                            });
                        });
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(egui::Color32::from_gray(12)))
                    .show(ctx, |ui| self.reader(ui, ctx));
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
    install_crash_log();
    let mut builtin = rustga::extensions::builtin();
    let source: Arc<dyn Source> = Arc::from(
        builtin
            .pop()
            .expect("at least one built-in source is required"),
    );

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
        Box::new(move |cc| Ok(Box::new(App::new(source, &cc.egui_ctx, window)))),
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

    /// An `App` without the window or the opening listing load, so the state
    /// machine can be stepped directly.
    fn test_app(source: Arc<dyn Source>, data_root: PathBuf) -> App {
        let (tx, rx) = mpsc::channel();
        App {
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
            anchor_page: None,
            reader_row_width: 1.0,
            reader_bar_visible: false,
            reader_bar_slide: 0.0,
            reader_bar_reveal_since: None,
            reader_window_width: None,
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
