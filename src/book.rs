//! Right-to-left spreads and their paper rendering, independent of scroll layout.

use eframe::egui::{self, pos2, vec2, Color32, Painter, Pos2, Rect, TextureHandle, Vec2};

use crate::{App, ImageSlot, MUTED};

const TURN_SECONDS: f64 = 0.48;
const WIDE_TURN_SECONDS: f64 = 0.28;
const LOOKAHEAD_PAGES: usize = 8;
const PREFETCH_WORKERS: usize = 6;
const UV: Rect = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
const PAPER: Color32 = Color32::from_rgb(248, 248, 245);
/// Gutter shading, darkest at the fold and easing out over more stops so the
/// falloff reads as a soft curve rather than a band.
const SPINE_STOPS: [u8; 8] = [48, 40, 31, 23, 15, 9, 4, 0];
const SWIPE_IDLE: f64 = 0.18;
const SWIPE_DISTANCE: f32 = 45.0;
/// How far a shadow slides across the page, in leaf widths, for each leaf width
/// the sheet stands above it. Light comes from above the reader and a little
/// from the right, so a shelf of the turning sheet casts down and across the
/// gutter, onto a page the sheet itself does not cover.
const SHADOW_X: f32 = -0.30;
const SHADOW_Y: f32 = 0.16;
const STACK_DEPTH_MAX: f32 = 16.0;
const STACK_FOOT_RATIO: f32 = 0.18;
/// A closed cover's card edge only shows while the cover is still nearly shut;
/// it fades out over this much of a turn so it never trails the lifting sheet.
const COVER_EDGE_FADE: f32 = 0.05;

#[derive(Default)]
pub(crate) struct Swipe {
    last: Option<f64>,
    travel: Vec2,
    used: bool,
}

impl Swipe {
    fn push(&mut self, now: f64, delta: Vec2, can_turn: bool) -> Option<bool> {
        if delta == Vec2::ZERO {
            return None;
        }
        if self.last.is_none_or(|last| now - last > SWIPE_IDLE) {
            *self = Self::default();
        }
        self.last = Some(now);
        if !can_turn {
            self.used = true;
        }
        if self.used {
            return None;
        }
        self.travel += vec2(delta.x, delta.y.abs());
        if self.travel.x.abs() >= SWIPE_DISTANCE && self.travel.x.abs() > self.travel.y * 1.5 {
            self.used = true;
            return Some(self.travel.x < 0.0);
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spread {
    Leaves {
        left: Option<usize>,
        right: Option<usize>,
    },
    Wide(usize),
}

impl Spread {
    fn at(index: usize, len: usize, wide: impl Fn(usize) -> bool) -> Self {
        if wide(index) {
            return Self::Wide(index);
        }
        if index == 0 {
            return Self::Leaves {
                left: Some(0),
                right: None,
            };
        }
        // Pair within fixed two-page slots. Splitting a slot around a wide scan
        // never reshuffles later pages when an image finishes loading.
        let right = 1 + (index - 1) / 2 * 2;
        let left = right + 1;
        Self::Leaves {
            left: (left < len && (index == left || !wide(left) && !wide(right))).then_some(left),
            right: (index == right || !wide(right)).then_some(right),
        }
    }

    fn sides(self) -> [Option<usize>; 2] {
        match self {
            Self::Leaves { left, right } => [left, right],
            Self::Wide(index) => [Some(index), None],
        }
    }

    fn first(self) -> usize {
        self.sides().into_iter().flatten().min().unwrap()
    }

    fn last(self) -> usize {
        self.sides().into_iter().flatten().max().unwrap()
    }

    fn pages(self) -> impl Iterator<Item = usize> {
        self.sides().into_iter().flatten()
    }

    fn is_cover(self) -> bool {
        matches!(
            self,
            Self::Leaves {
                left: Some(0),
                right: None
            }
        )
    }

    fn footprint(self) -> [f32; 2] {
        match self {
            Self::Wide(_) => [1.0, 1.0],
            Self::Leaves { left, right } => [f32::from(left.is_some()), f32::from(right.is_some())],
        }
    }

    /// Shift the hinge so a lone leaf, rather than the empty gutter, is centered.
    fn hinge_shift(self) -> f32 {
        let [left, right] = self.footprint();
        (left - right) * 0.5
    }

    fn label(self, count: usize) -> String {
        match (self.first(), self.last()) {
            (0, _) => "Cover".to_owned(),
            (first, last) if first == last => format!("{first} / {count}"),
            (first, last) => format!("{first}–{last} / {count}"),
        }
    }
}

/// What the reader bar needs to render spread navigation.
struct BookNav {
    next: Option<Spread>,
    previous: Option<Spread>,
    label: String,
    can_turn: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Turn {
    from: Spread,
    to: Spread,
    forward: bool,
    started: f64,
    from_progress: f32,
    to_progress: f32,
    corner: f32,
}

impl Turn {
    fn duration(self) -> f64 {
        let full = if matches!(self.from, Spread::Wide(_)) || matches!(self.to, Spread::Wide(_)) {
            WIDE_TURN_SECONDS
        } else {
            TURN_SECONDS
        };
        full * f64::from((self.to_progress - self.from_progress).abs().max(0.25))
    }

    /// The closed cover's card edge only shows while the cover is at rest or
    /// nearly so: it fades out over the first sliver of an opening turn and
    /// back in over the last sliver of a closing one.
    fn cover_edge_opacity(self, t: f32) -> Option<f32> {
        let opening = if self.from.is_cover() {
            t
        } else if self.to.is_cover() {
            1.0 - t
        } else {
            return None;
        };
        Some(1.0 - smooth((opening / COVER_EDGE_FADE).clamp(0.0, 1.0)))
    }

    /// The gutter forms as the book opens, on both the lifted and resting leaves.
    fn spine_profile(self, t: f32) -> f32 {
        if self.from.is_cover() {
            smooth(t)
        } else if self.to.is_cover() {
            smooth(1.0 - t)
        } else {
            1.0
        }
    }
}

impl App {
    pub(crate) fn book_reader(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Book leaf zero is the cover; chapter pages keep their original indices
        // in the loader and the scroll reader.
        let len = self.pages.len() + 1;
        let now = ctx.input(|input| input.time);
        let page = self.anchor_page.take().unwrap_or(self.current_page);
        let cover_url = self
            .series
            .as_ref()
            .and_then(|series| series.thumbnail_url.clone());
        if let Some(url) = &cover_url {
            self.load_cover(url, ctx);
        }
        let finished_turn = self
            .book_turn
            .is_some_and(|turn| now - turn.started >= turn.duration());
        if finished_turn {
            if let Some(turn) = self.book_turn.take() {
                if turn.to_progress == 1.0 {
                    self.book_cover_visible = turn.to.first() == 0;
                    self.current_page = turn.to.first().saturating_sub(1);
                }
            }
        }
        let page = if finished_turn {
            self.current_page
        } else {
            page
        };
        let current = if self.book_cover_visible && page == 0 {
            0
        } else {
            (page + 1).min(len - 1)
        };
        let spread = self.spread_at(current);
        let previous = spread
            .first()
            .checked_sub(1)
            .map(|index| self.spread_at(index));
        let next = (spread.last() + 1 < len).then(|| self.spread_at(spread.last() + 1));
        let (area, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let painter = ui.painter().clone();
        // The bar floats over the bottom edge, so keep the spread clear of it.
        let bottom = area.bottom() - 24.0;
        let center = pos2(area.center().x, (area.top() + bottom) * 0.5);
        let ratio = self.book_texture(0).map_or(0.7, |texture| {
            (texture.size_vec2().x / texture.size_vec2().y).clamp(0.55, 0.8)
        });
        let height = (area.width() * 0.5 / ratio - 32.0)
            .min(bottom - area.top() - 24.0)
            .max(1.0)
            * self.reader_settings.scale;
        let spread_book = Book {
            painter: &painter,
            center,
            leaf: vec2(height * ratio, height),
            spine_profile: 1.0,
            behind: leaves_around(spread, self.pages.len()),
        };
        let book = spread_book.shift_hinge(spread.hinge_shift());

        let popup = egui::Popup::is_any_open(ctx);
        if self.book_turn.is_none() && self.book_pending_turn.is_none() && !popup {
            for (forward, target) in [(true, next), (false, previous)] {
                let Some(to) = target else { continue };
                if matches!(spread, Spread::Wide(_)) || matches!(to, Spread::Wide(_)) {
                    continue;
                }
                let side = usize::from(!forward);
                let rect = book.side(side);
                let corner = if forward {
                    rect.left_bottom()
                } else {
                    rect.right_bottom()
                };
                let size = vec2(
                    (rect.width() * 0.30).clamp(1.0, 110.0),
                    (rect.height() * 0.25).clamp(1.0, 120.0),
                );
                let min = pos2(
                    if forward { corner.x } else { corner.x - size.x },
                    corner.y - size.y,
                );
                let response = ui.interact(
                    Rect::from_min_size(min, size),
                    ui.id().with(("page-corner", side)),
                    egui::Sense::drag(),
                );
                let ready = spread.sides()[side].is_some()
                    && spread
                        .pages()
                        .chain(to.pages())
                        .all(|leaf| self.book_texture(leaf).is_some());
                if ready {
                    if response.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                    if response.drag_started() {
                        self.book_drag = Some((
                            Turn {
                                from: spread,
                                to,
                                forward,
                                started: now,
                                from_progress: 0.0,
                                to_progress: 1.0,
                                corner: 0.3,
                            },
                            0.0,
                        ));
                    }
                }
                if let Some((turn, progress)) = &mut self.book_drag {
                    if turn.forward == forward && (response.dragged() || response.drag_stopped()) {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                        // Use the total distance from the grab, not the last frame's delta.
                        let movement = ctx.input(|input| {
                            match (input.pointer.press_origin(), input.pointer.interact_pos()) {
                                (Some(start), Some(pointer)) => pointer - start,
                                _ => Vec2::ZERO,
                            }
                        });
                        turn.corner = (0.3 - movement.y / rect.height()).clamp(0.0, 0.85);
                        if response.dragged() || movement != Vec2::ZERO {
                            *progress = (movement.x * if forward { 1.0 } else { -1.0 }
                                / (rect.width() * 1.6))
                                .clamp(0.0, 0.99);
                        }
                    }
                }
            }
        }
        let release = popup || !ctx.input(|input| input.pointer.primary_down());
        if release {
            if let Some((mut turn, progress)) = self.book_drag.take() {
                turn.started = now;
                turn.from_progress = progress;
                turn.to_progress = if progress >= 0.40 && !popup { 1.0 } else { 0.0 };
                self.book_turn = Some(turn);
                ctx.request_repaint();
            }
        }
        if std::env::var_os("RUSTGA_BOOK_DEBUG").is_some() {
            log_spread(ctx, self, spread);
        }
        if let Some(turn) = self.book_turn {
            let t = ((now - turn.started) / turn.duration()).clamp(0.0, 1.0) as f32;
            spread_book.turn(
                self,
                turn,
                egui::lerp(turn.from_progress..=turn.to_progress, smooth(t)),
            );
            ctx.request_repaint();
        } else if let Some((turn, progress)) =
            self.book_drag.filter(|(_, progress)| *progress > 0.0)
        {
            spread_book.turn(self, turn, progress);
        } else {
            book.spread(self, spread);
            for leaf in spread.pages() {
                let rect = book.page_rect(spread, leaf);
                let Some(index) = leaf.checked_sub(1) else {
                    if let Some(url) = &cover_url {
                        if let Some(ImageSlot::Failed(message)) = self.covers.get(url) {
                            if ui
                                .put(
                                    Rect::from_center_size(
                                        rect.center_bottom() - vec2(0.0, 28.0),
                                        vec2(rect.width().min(170.0), 28.0),
                                    ),
                                    egui::Button::new("Retry cover"),
                                )
                                .on_hover_text(message)
                                .clicked()
                            {
                                self.covers.remove(url);
                                self.load_cover(url, ctx);
                            }
                        }
                    }
                    continue;
                };
                match &self.images[index] {
                    ImageSlot::Failed(message) => {
                        if ui
                            .put(
                                Rect::from_center_size(
                                    rect.center(),
                                    vec2(rect.width().min(170.0), 28.0),
                                ),
                                egui::Button::new(format!("Retry page {}", index + 1)),
                            )
                            .on_hover_text(message.as_str())
                            .clicked()
                        {
                            self.images[index] = ImageSlot::Empty;
                            self.load_image(index, ctx);
                            ctx.request_repaint();
                            return;
                        }
                    }
                    ImageSlot::Loading | ImageSlot::Empty => {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            format!("Loading page {}…", index + 1),
                            egui::FontId::proportional(12.0),
                            MUTED,
                        );
                    }
                    ImageSlot::Ready(_) => {}
                }
            }
        }

        let mut forward = false;
        let mut backward = false;
        let can_turn = self.book_turn.is_none()
            && self.book_drag.is_none()
            && self.book_pending_turn.is_none();
        let accepts_input = can_turn && !egui::Popup::is_any_open(ctx);
        let (swipe, left, right) = ctx.input(|input| {
            let swipe = input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta,
                        modifiers,
                    } if !modifiers.any() => Some(*delta),
                    _ => None,
                })
                .fold(Vec2::ZERO, |sum, delta| sum + delta);
            (
                swipe,
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
            )
        });
        let swipe = self.book_swipe.push(now, swipe, accepts_input);
        if accepts_input {
            forward |= left || swipe == Some(true);
            backward |= right || swipe == Some(false);
        }
        let direction = self.book_pending_turn.or_else(|| {
            if forward && next.is_some() {
                Some(true)
            } else if backward && previous.is_some() {
                Some(false)
            } else {
                None
            }
        });
        let target = direction.and_then(|forward| if forward { next } else { previous });
        self.current_page = current.saturating_sub(1);
        let pending_before = self.book_pending_turn.take();
        if let Some((forward, to)) = direction.zip(target) {
            // A failed image can still be retried on its spread. Only an active
            // load should hold the turn, never a missing cover or a failed page.
            if spread.pages().chain(to.pages()).any(|leaf| {
                leaf > 0 && matches!(self.images[leaf - 1], ImageSlot::Empty | ImageSlot::Loading)
            }) {
                self.book_pending_turn = Some(forward);
            } else {
                self.book_turn = Some(Turn {
                    from: spread,
                    to,
                    forward,
                    started: now,
                    from_progress: 0.0,
                    to_progress: 1.0,
                    corner: 0.3,
                });
                ctx.request_repaint();
            }
        }
        if self.book_pending_turn != pending_before {
            ctx.request_repaint();
        }
        // Let the visible and requested pages claim slots before reading ahead.
        let ahead = (spread.last() + 1..len).take(LOOKAHEAD_PAGES);
        for leaf in spread
            .pages()
            .chain(target.into_iter().flat_map(Spread::pages))
            .chain(ahead.clone().take(2))
        {
            if let Some(index) = leaf.checked_sub(1) {
                self.load_image(index, ctx);
            }
        }
        // Leave worker capacity for pages requested by the next turn. An
        // in-flight speculative download cannot be reprioritized.
        if self.book_pending_turn.is_none() {
            for leaf in ahead
                .skip(2)
                .chain(previous.into_iter().flat_map(Spread::pages))
            {
                if self.image_in_flight >= PREFETCH_WORKERS {
                    break;
                }
                if let Some(index) = leaf.checked_sub(1) {
                    self.load_image(index, ctx);
                }
            }
        }
        // Retain both neighbours and the full read-ahead window, including a turn's faces.
        for (index, slot) in self.images.iter_mut().enumerate() {
            let leaf = index + 1;
            if (leaf + LOOKAHEAD_PAGES < spread.first() || leaf > spread.last() + LOOKAHEAD_PAGES)
                && matches!(slot, ImageSlot::Ready(_))
            {
                *slot = ImageSlot::Empty;
            }
        }
    }

    /// The spread being shown, from the committed reader position.
    fn book_spread(&self) -> Spread {
        let len = self.pages.len() + 1;
        let current = if self.book_cover_visible && self.current_page == 0 {
            0
        } else {
            (self.current_page + 1).min(len - 1)
        };
        self.spread_at(current)
    }

    /// A spread at a chapter index, keeping wide scans on a page of their own.
    fn spread_at(&self, index: usize) -> Spread {
        let len = self.pages.len() + 1;
        Spread::at(index, len, |page| {
            page != 0
                && self
                    .page_dimensions
                    .get(page - 1)
                    .and_then(|size| *size)
                    .is_some_and(|size| size.x >= size.y * 1.15)
        })
    }

    fn book_nav(&self) -> Option<BookNav> {
        if self.pages.is_empty() {
            return None;
        }
        let len = self.pages.len() + 1;
        let spread = self.book_spread();
        let previous = spread
            .first()
            .checked_sub(1)
            .map(|index| self.spread_at(index));
        let next = (spread.last() + 1 < len).then(|| self.spread_at(spread.last() + 1));
        let loading = spread
            .pages()
            .filter(|&leaf| leaf > 0)
            .find(|&leaf| self.book_texture(leaf).is_none());
        let label = if let Some(leaf) = loading {
            format!("Loading page {leaf}…")
        } else if self.book_pending_turn.is_some() {
            "Loading spread…".to_owned()
        } else {
            spread.label(len - 1)
        };
        Some(BookNav {
            can_turn: self.book_turn.is_none()
                && self.book_drag.is_none()
                && self.book_pending_turn.is_none(),
            next,
            previous,
            label,
        })
    }

    /// Spread navigation in the reader bar. The turn itself is left to the
    /// canvas, which owns the texture gating and the animation.
    pub(crate) fn book_bar_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(nav) = self.book_nav() else {
            return;
        };
        let mut step = None;
        if ui
            .add_enabled(
                nav.can_turn && nav.next.is_some(),
                egui::Button::new(egui::RichText::new("← Spread").size(12.5)),
            )
            .clicked()
        {
            step = Some(true);
        }
        ui.label(egui::RichText::new(nav.label).size(11.5).color(MUTED));
        if ui
            .add_enabled(
                nav.can_turn && nav.previous.is_some(),
                egui::Button::new(egui::RichText::new("Spread →").size(12.5)),
            )
            .clicked()
        {
            step = Some(false);
        }
        if let Some(forward) = step {
            self.book_pending_turn = Some(forward);
            ctx.request_repaint();
        }
    }

    fn book_texture(&self, leaf: usize) -> Option<&TextureHandle> {
        let slot = if let Some(index) = leaf.checked_sub(1) {
            self.images.get(index)
        } else {
            let url = self.series.as_ref()?.thumbnail_url.as_deref()?;
            self.covers.get(url)
        };
        match slot {
            Some(ImageSlot::Ready(texture)) => Some(texture),
            _ => None,
        }
    }
}

struct Book<'a> {
    painter: &'a Painter,
    center: Pos2,
    leaf: Vec2,
    spine_profile: f32,
    /// Sheets actually left on each side, counting pages front and back.
    behind: [usize; 2],
}

impl Book<'_> {
    fn shift_hinge(&self, leaves: f32) -> Self {
        Self {
            center: self.center + vec2(leaves * self.leaf.x, 0.0),
            ..*self
        }
    }

    fn side(&self, side: usize) -> Rect {
        Rect::from_center_size(
            self.center + vec2((side as f32 - 0.5) * self.leaf.x, 0.0),
            self.leaf,
        )
    }

    fn page_rect(&self, spread: Spread, index: usize) -> Rect {
        match spread {
            Spread::Wide(_) => self.side(0).union(self.side(1)),
            Spread::Leaves { left, .. } => self.side(usize::from(left != Some(index))),
        }
    }

    fn stack(&self, side: usize, rect: Rect, amount: f32) {
        if amount <= 0.0 {
            return;
        }
        let mut painter = self.painter.clone();
        painter.multiply_opacity(amount);
        let outward = if side == 0 { -1.0 } else { 1.0 };
        // One visible edge per sheet that is really there. A chapter's closing
        // spread has a single sheet left, so it shows a single edge instead of
        // a fan of pages that do not exist.
        let layers = self.behind[side].min(24);
        if layers == 0 {
            return;
        }
        let depth = (self.behind[side] as f32 * 0.45).min(STACK_DEPTH_MAX)
            * (self.leaf.x / 260.0).min(1.0)
            * amount;
        let lip = depth + 2.0 * amount;
        let inner = if side == 0 { rect.left() } else { rect.right() };
        let outer = inner + outward * lip;
        let halo = 3.0 * amount;
        // Draw only the cut edges, not a backing rectangle that would show
        // through when the turning leaf lifts.
        let foot = depth * STACK_FOOT_RATIO;
        let mut bottom = egui::Mesh::default();
        for (column, (u, taper)) in [(0.54, 0.0), (0.73, 0.38), (0.91, 1.0), (1.0, 0.35)]
            .into_iter()
            .enumerate()
        {
            let x = inner - outward * (1.0 - u) * rect.width();
            for (v, color) in [
                (0.0, Color32::from_rgb(192, 187, 178)),
                (0.40, Color32::from_rgb(223, 216, 203)),
                (0.68, Color32::from_rgb(202, 194, 181)),
                (0.80, Color32::from_rgb(215, 207, 194)),
                (0.85, Color32::from_rgb(135, 124, 111)),
                (1.0, Color32::from_rgb(111, 102, 92)),
            ] {
                bottom.colored_vertex(pos2(x, rect.bottom() + v * foot * taper), color);
            }
            if column > 0 {
                for row in 0..5 {
                    grid_quad(&mut bottom, (column as u32 - 1) * 6 + row, 6);
                }
            }
        }
        painter.add(bottom);
        let (edge_min, edge_max) = if side == 0 {
            (outer - halo, rect.left())
        } else {
            (rect.right(), outer + halo)
        };
        let edge = Rect::from_min_max(pos2(edge_min, rect.top()), pos2(edge_max, rect.bottom()));
        let painter = painter.with_clip_rect(edge);
        // A narrow halo seats the block against the background without the hard
        // rectangle a drop shadow leaves behind.
        let tuck = (lip * 0.32).min(rect.height() * 0.018);
        let stack_y = |offset: f32| rect.y_range().shrink(tuck * offset);
        let rim = stack_y(1.0);
        let mut halo_mesh = egui::Mesh::default();
        for (step, alpha) in [(0.0, 52), (0.55, 22), (1.0, 0)] {
            let x = outer + outward * halo * step;
            halo_mesh.colored_vertex(pos2(x, rim.min), Color32::from_black_alpha(alpha));
            halo_mesh.colored_vertex(pos2(x, rim.max), Color32::from_black_alpha(alpha));
        }
        quad(&mut halo_mesh, 0);
        quad(&mut halo_mesh, 2);
        painter.add(halo_mesh);
        // The cut paper catches light just past the top leaf's thin shadow.
        let paper = |t: f32| {
            let light = egui::lerp(241.0..=208.0, smooth(t)) - 30.0 * (1.0 - t / 0.18).max(0.0);
            let shade = |drop: f32| (light - drop).round() as u8;
            Color32::from_rgb(shade(0.0), shade(2.0), shade(8.0))
        };
        let mut mesh = egui::Mesh::default();
        let steps = 6;
        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let x = inner + outward * lip * t;
            let color = paper(t);
            let y = stack_y(t);
            mesh.colored_vertex(pos2(x, y.min), color);
            mesh.colored_vertex(pos2(x, y.max), color);
            if step > 0 {
                quad(&mut mesh, (step as u32 - 1) * 2);
            }
        }
        painter.add(mesh);
        // Sheets read as hairline striations, never as bands. Skip any line that
        // would land on a device pixel its neighbour already covers.
        let pixel = 1.0 / painter.ctx().pixels_per_point();
        let pitch = (depth / layers as f32).max(pixel * 1.35);
        for line in 1..=layers {
            let offset = (line as f32 - 0.4) * pitch;
            let fade = ((depth - offset) / pitch).clamp(0.0, 1.0);
            if fade == 0.0 {
                break;
            }
            let x = inner + outward * offset;
            let y = stack_y(offset / lip);
            let alpha = if line % 3 == 0 { 39.0 } else { 23.0 };
            painter.line_segment(
                [pos2(x, y.min), pos2(x, y.max)],
                egui::Stroke::new(
                    pixel,
                    Color32::from_rgba_unmultiplied(94, 81, 69, (alpha * fade) as u8),
                ),
            );
        }
    }

    fn contact_shadow(&self, footprint: [f32; 2]) {
        let height = (self.leaf.x * 0.025).clamp(2.0, 12.0);
        let bottom = self.center.y + self.leaf.y * 0.5;
        let spine = footprint[0].min(footprint[1]);
        let mut mesh = egui::Mesh::default();
        for (side, amount) in footprint.into_iter().enumerate() {
            if amount <= 0.0 {
                continue;
            }
            let outward = if side == 0 { -1.0 } else { 1.0 };
            let first = mesh.vertices.len() as u32;
            // Match alpha at the seam and feather into the footprint when only one side exists.
            for (column, (u, strength)) in [
                (0.0, spine),
                (0.04, amount),
                (0.45, 0.45 * amount),
                (1.0, 0.0),
            ]
            .into_iter()
            .enumerate()
            {
                let x = self.center.x + outward * u * self.leaf.x;
                // A narrow contact core gives way to a softer tail cast down-left.
                for (v, alpha) in [(0.0, 43.0), (0.20, 22.0), (0.62, 10.0), (1.35, 0.0)] {
                    mesh.colored_vertex(
                        pos2(x + SHADOW_X * v * height, bottom + v * height),
                        Color32::from_black_alpha((alpha * strength).round() as u8),
                    );
                }
                if column > 0 {
                    let prev = first + (column as u32 - 1) * 4;
                    for row in 0..3 {
                        grid_quad(&mut mesh, prev + row, 4);
                    }
                }
            }
        }
        self.painter.add(mesh);
    }

    fn spread(&self, app: &App, spread: Spread) {
        self.contact_shadow(spread.footprint());
        if spread.is_cover() {
            self.cover_edge(self.side(0), 1.0);
        }
        self.paint_spread(app, spread);
    }

    /// The leaves of a spread without the closed cover's card edge. Turns draw
    /// that edge themselves so it is neither clipped by a reveal nor drawn twice.
    fn paint_spread(&self, app: &App, spread: Spread) {
        match spread {
            Spread::Wide(index) => {
                for side in 0..2 {
                    self.stack(side, self.side(side), 1.0);
                }
                self.flat(app, self.page_rect(spread, index), Some(index));
            }
            Spread::Leaves { left, right } => {
                for (side, page) in [left, right].into_iter().enumerate() {
                    if page.is_some() {
                        let rect = self.side(side);
                        if left.is_some() && right.is_some() {
                            self.stack(side, rect, 1.0);
                        }
                        self.flat(app, rect, page);
                    }
                }
            }
        }
    }

    fn flat(&self, app: &App, rect: Rect, page: Option<usize>) {
        // Paper pages bow; a cover leaf keeps its flat card profile.
        let profile = if page == Some(0) {
            0.0
        } else {
            self.spine_profile
        };
        let Some(texture) = page.and_then(|index| app.book_texture(index)) else {
            // A page that has not decoded must not paint as a blank white leaf.
            if paints_leaf(page, false) {
                self.paper(rect, profile);
                if page == Some(0) {
                    self.cover_title(app, rect);
                }
            }
            return;
        };
        {
            // Fill the paper so the image bends all the way to both gutter edges.
            let art = rect;
            if page == Some(0) {
                self.painter.image(texture.id(), art, UV, Color32::WHITE);
            } else {
                // Include the exact gutter column so a wide image stays one
                // continuous texture without a seam or a duplicated strip.
                let steps = self.curve_steps(art.width());
                let mut columns = (0..=steps)
                    .map(|i| egui::lerp(art.x_range(), i as f32 / steps as f32))
                    .collect::<Vec<_>>();
                if art.x_range().contains(self.center.x) {
                    columns.push(self.center.x);
                    columns.sort_by(f32::total_cmp);
                    columns.dedup();
                }
                let mut mesh = egui::Mesh::with_texture(texture.id());
                for (i, x) in columns.into_iter().enumerate() {
                    for (v, y) in [(0.0, art.top()), (1.0, art.bottom())] {
                        mesh.vertices.push(egui::epaint::Vertex {
                            pos: pos2(
                                hinge_overlap(
                                    x,
                                    self.center.x,
                                    rect.center().x - self.center.x,
                                    0.75 / self.painter.ctx().pixels_per_point(),
                                ),
                                y + self.gutter_offset(x, profile) * (1.0 - 2.0 * v),
                            ),
                            uv: pos2((x - art.left()) / art.width(), v),
                            color: self.surface_color(Color32::WHITE, x, profile),
                        });
                    }
                    if i > 0 {
                        quad(&mut mesh, (i as u32 - 1) * 2);
                    }
                }
                feather_page_edges(&mut mesh, 1.0 / self.painter.ctx().pixels_per_point());
                self.painter
                    .with_clip_rect(rect.expand2(vec2(1.0, 3.0)))
                    .add(mesh);
            }
        }
    }

    /// The cut edge of a closed cover: a hairline of card stock just outside the
    /// artwork, scaled with the rendered cover and kept off the empty half.
    fn cover_edge(&self, rect: Rect, opacity: f32) {
        if opacity <= 0.0 {
            return;
        }
        let thickness = (rect.width() * 0.006).clamp(1.0, 2.5);
        let outer = rect.left() - thickness;
        let mut painter = self.painter.clone();
        painter.multiply_opacity(opacity);
        let mut mesh = egui::Mesh::default();
        for (step, color) in [
            (0.0, Color32::from_rgb(168, 164, 156)),
            (0.6, Color32::from_rgb(198, 195, 187)),
            (1.0, Color32::from_rgb(214, 211, 204)),
        ] {
            let x = outer + thickness * step;
            mesh.colored_vertex(pos2(x, rect.top()), color);
            mesh.colored_vertex(pos2(x, rect.bottom()), color);
        }
        quad(&mut mesh, 0);
        quad(&mut mesh, 2);
        painter.add(mesh);
    }

    fn cover_title(&self, app: &App, rect: Rect) {
        let title = app
            .series
            .as_ref()
            .map_or("Series cover", |series| &series.title);
        self.painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            title,
            egui::FontId::proportional(16.0),
            Color32::from_gray(64),
        );
    }

    fn paper(&self, rect: Rect, profile: f32) {
        let mut mesh = egui::Mesh::default();
        let steps = self.curve_steps(rect.width());
        for i in 0..=steps {
            let x = egui::lerp(rect.x_range(), i as f32 / steps as f32);
            mesh.colored_vertex(
                pos2(x, rect.top() + self.gutter_offset(x, profile)),
                self.surface_color(PAPER, x, profile),
            );
            mesh.colored_vertex(
                pos2(x, rect.bottom() - self.gutter_offset(x, profile)),
                self.surface_color(PAPER, x, profile),
            );
            if i > 0 {
                quad(&mut mesh, (i - 1) * 2);
            }
        }
        feather_page_edges(&mut mesh, 1.0 / self.painter.ctx().pixels_per_point());
        self.painter
            .with_clip_rect(rect.expand2(vec2(1.0, 3.0)))
            .add(mesh);
    }

    fn curve_steps(&self, width: f32) -> u32 {
        (width * self.painter.ctx().pixels_per_point() / 2.0)
            .ceil()
            .clamp(32.0, 512.0) as u32
    }

    fn gutter_offset(&self, x: f32, profile: f32) -> f32 {
        page_offset(
            (x - self.center.x).abs(),
            self.leaf.x,
            self.spine_width(),
            profile,
        )
    }

    fn spine_width(&self) -> f32 {
        (self.leaf.x * 0.12).min(38.0)
    }

    fn surface_color(&self, base: Color32, x: f32, profile: f32) -> Color32 {
        let light = 1.0 - profile * spine_alpha((x - self.center.x).abs() / self.spine_width());
        let shade = |channel: u8| (f32::from(channel) * light).round() as u8;
        Color32::from_rgb(shade(base.r()), shade(base.g()), shade(base.b()))
    }

    fn turn(&self, app: &App, turn: Turn, t: f32) {
        let shift = egui::lerp(turn.from.hinge_shift()..=turn.to.hinge_shift(), smooth(t));
        let mut book = self.shift_hinge(shift);
        book.spine_profile = turn.spine_profile(t);
        book.paint_turn(app, turn, t);
    }

    fn paint_turn(&self, app: &App, turn: Turn, t: f32) {
        let [old_left, old_right] = turn.from.footprint();
        let [new_left, new_right] = turn.to.footprint();
        self.contact_shadow([
            egui::lerp(old_left..=new_left, t),
            egui::lerp(old_right..=new_right, t),
        ]);
        let Some(faces) = sheet_faces(turn) else {
            // Keep the physical book visible while a double-page scan is revealed.
            self.paint_spread(app, turn.from);
            let full = self.side(0).union(self.side(1));
            let edge = if turn.forward {
                egui::lerp(full.left()..=full.right(), t)
            } else {
                egui::lerp(full.right()..=full.left(), t)
            };
            let reveal = if turn.forward {
                Rect::from_min_max(full.min, pos2(edge, full.bottom()))
            } else {
                Rect::from_min_max(pos2(edge, full.top()), full.max)
            };
            // Let the bowed leaf and the bottom cut edge through without moving
            // the horizontal wipe. The parent clip still bounds both.
            let room = if turn.to.is_cover() {
                0.0
            } else {
                page_bow(self.leaf.x) + 1.0 / self.painter.ctx().pixels_per_point()
            };
            let reveal = Rect::from_min_max(
                reveal.min - vec2(0.0, room),
                reveal.max
                    + vec2(
                        0.0,
                        if turn.to.is_cover() {
                            0.0
                        } else {
                            room.max(STACK_DEPTH_MAX * STACK_FOOT_RATIO)
                        },
                    ),
            )
            .intersect(self.painter.clip_rect());
            let painter = self.painter.with_clip_rect(reveal);
            Book {
                painter: &painter,
                behind: leaves_around(turn.to, app.pages.len()),
                ..*self
            }
            .paint_spread(app, turn.to);
            // The card edge sits outside the page bounds, so it also sits
            // outside the reveal clip and uses the same near-closed fade.
            if let Some(opacity) = turn.cover_edge_opacity(t) {
                self.cover_edge(self.side(0), opacity);
            }
            return;
        };
        let outgoing = usize::from(!turn.forward);
        let incoming = 1 - outgoing;
        let old = turn.from.sides();
        let new = turn.to.sides();
        let old_leaves = leaves_around(turn.from, app.pages.len());
        let new_leaves = leaves_around(turn.to, app.pages.len());
        let stack = Book {
            behind: [
                egui::lerp(old_leaves[0] as f32..=new_leaves[0] as f32, t).round() as usize,
                egui::lerp(old_leaves[1] as f32..=new_leaves[1] as f32, t).round() as usize,
            ],
            ..*self
        };
        let cover_turn = old.contains(&Some(0)) || new.contains(&Some(0));
        let old_stack = old.iter().all(Option::is_some);
        let new_stack = new.iter().all(Option::is_some);
        // The same near-closed fade as the reveal path, still drawn once outside
        // any clipping so a canceled drag cannot leave the edge behind.
        if let Some(opacity) = turn.cover_edge_opacity(t) {
            self.cover_edge(self.side(0), opacity);
        }
        for (side, page) in [(outgoing, new[outgoing]), (incoming, old[incoming])] {
            let rect = self.side(side);
            let amount = if page.is_some() && old[side].is_some() && new[side].is_some() {
                egui::lerp(f32::from(old_stack)..=f32::from(new_stack), t)
            } else {
                // No stationary backing: only show edges when a real leaf is
                // almost flat on this side, never on an empty half mid-turn.
                let leaving = if old_stack && old[side].is_some() {
                    1.0 - 4.0 * t
                } else {
                    0.0
                };
                let arriving = if new_stack && new[side].is_some() {
                    4.0 * t - 3.0
                } else {
                    0.0
                };
                let amount = leaving.max(arriving).max(0.0);
                smooth(amount)
            };
            // An empty half gets nothing at all: no edge, no shadow. Anything
            // drawn there reads as a phantom page while the sheet is away.
            let Some(page) = page else { continue };
            stack.stack(side, rect, amount);
            self.flat(app, rect, Some(page));
        }
        // One shadow for the whole sheet, drawn under it and over both pages.
        for (side, page) in [(outgoing, new[outgoing]), (incoming, old[incoming])] {
            if page.is_some() {
                let painter = self.painter.with_clip_rect(self.side(side));
                Book {
                    painter: &painter,
                    ..*self
                }
                .cast_shadow(outgoing, t, cover_turn, turn.corner);
            }
        }
        self.curl(app, faces, outgoing, turn, t);
    }

    /// The shadow the turning sheet throws on the pages.
    ///
    /// Every strip of the sheet projects its shadow along the light, so the
    /// shadow falls beside the sheet rather than under it, where the sheet
    /// itself would hide it. Blur grows with height: the fold keeps a tight
    /// contact line while a lifted edge spreads and fades, and a sheet lying
    /// flat casts nothing visible because its shadow is its own footprint.
    fn cast_shadow(&self, outgoing: usize, t: f32, cover: bool, corner: f32) {
        let direction = if outgoing == 0 { -1.0 } else { 1.0 };
        // Several nearby light samples form a soft shadow that broadens with height.
        for (dx, dy) in [
            (0.0, 0.0),
            (-0.055, 0.0),
            (0.055, 0.0),
            (0.0, -0.055),
            (0.0, 0.055),
        ] {
            let mut mesh = egui::Mesh::default();
            for row in 0..=16 {
                let v = row as f32 / 16.0;
                for column in 0..=48 {
                    let u = column as f32 / 48.0;
                    let (x, y, z) = sheet_point(u, v, t, cover, corner);
                    let pos = self.center
                        + vec2(
                            (direction * x + (SHADOW_X + dx) * z) * self.leaf.x,
                            y * self.leaf.y + (SHADOW_Y + dy) * z * self.leaf.x,
                        );
                    // A sheet touching the paper has no exposed cast shadow.
                    // In particular, its bound edge must not acquire a dark seam.
                    mesh.colored_vertex(pos, Color32::from_black_alpha(lifted_shadow_alpha(z)));
                    if row > 0 && column > 0 {
                        grid_quad(&mut mesh, (row - 1) * 49 + column - 1, 49);
                    }
                }
            }
            self.painter.add(mesh);
        }
    }

    fn curl(&self, app: &App, pages: [Option<usize>; 2], outgoing: usize, turn: Turn, t: f32) {
        let direction = if outgoing == 0 { -1.0 } else { 1.0 };
        let cover = turn.from.is_cover() || turn.to.is_cover();
        let corner = turn.corner;
        // Draw rows independently: the held corner leads the top of the sheet.
        let mut cells = Vec::new();
        for row in 0..16 {
            for column in 0..72 {
                let u = column as f32 / 72.0;
                let v = row as f32 / 16.0;
                let z = sheet_point(u + 0.5 / 72.0, v + 0.5 / 16.0, t, cover, corner).2;
                cells.push((u, v, z));
            }
        }
        cells.sort_by(|a, b| a.2.total_cmp(&b.2));
        for (u, v, _) in cells {
            let next_u = u + 1.0 / 72.0;
            let next_v = v + 1.0 / 16.0;
            let start = sheet_point(u, v, t, cover, corner);
            let end = sheet_point(next_u, v, t, cover, corner);
            let reverse = end.0 < start.0;
            let Some(page) = pages[usize::from(reverse)] else {
                continue;
            };
            let texture = app.book_texture(page);
            let art_rect = UV;
            let mut us = vec![u, next_u];
            let mut vs = vec![v, next_v];
            for edge in [art_rect.left(), art_rect.right()] {
                if edge > u && edge < next_u {
                    us.push(edge);
                }
            }
            for edge in [art_rect.top(), art_rect.bottom()] {
                if edge > v && edge < next_v {
                    vs.push(edge);
                }
            }
            us.sort_by(f32::total_cmp);
            vs.sort_by(f32::total_cmp);
            for xs in us.windows(2) {
                for ys in vs.windows(2) {
                    let textured =
                        art_rect.contains(pos2((xs[0] + xs[1]) * 0.5, (ys[0] + ys[1]) * 0.5));
                    let texture = texture.filter(|_| textured);
                    let mut mesh = egui::Mesh::with_texture(
                        texture.map_or(egui::TextureId::default(), TextureHandle::id),
                    );
                    for u in xs {
                        for v in ys {
                            let (x, y, z) = sheet_point(*u, *v, t, cover, corner);
                            let ahead = sheet_point((*u + 0.001).min(1.0), *v, t, cover, corner);
                            let behind = sheet_point((*u - 0.001).max(0.0), *v, t, cover, corner);
                            let normal =
                                vec2(-direction * (ahead.2 - behind.2), ahead.0 - behind.0)
                                    .normalized();
                            let normal = if reverse { -normal } else { normal };
                            let light = vec2(SHADOW_X, 1.0).normalized();
                            let crease = if page == 0 {
                                0.0
                            } else {
                                self.spine_profile
                                    * spine_alpha(x.abs() * self.leaf.x / self.spine_width())
                                    * (1.0 - z.clamp(0.0, 1.0))
                            };
                            let brightness = (0.72
                                + 0.28 * (normal.dot(light).max(0.0) / light.y).min(1.0))
                                * (1.0 - crease);
                            let base = if texture.is_some() {
                                Color32::WHITE
                            } else {
                                PAPER
                            };
                            let shade = |c: u8| (f32::from(c) * brightness).round() as u8;
                            let art_x = if (if reverse { -direction } else { direction }) > 0.0 {
                                *u
                            } else {
                                1.0 - *u
                            };
                            let uv = if texture.is_some() {
                                pos2(
                                    (art_x - art_rect.left()) / art_rect.width(),
                                    (*v - art_rect.top()) / art_rect.height(),
                                )
                            } else {
                                egui::epaint::WHITE_UV
                            };
                            let mut pos =
                                self.center + vec2(direction * x * self.leaf.x, y * self.leaf.y);
                            pos.y += self.gutter_offset(pos.x, self.spine_profile)
                                * (1.0 - z.min(1.0))
                                * (1.0 - 2.0 * v);
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos,
                                uv,
                                color: Color32::from_rgb(
                                    shade(base.r()),
                                    shade(base.g()),
                                    shade(base.b()),
                                ),
                            });
                        }
                    }
                    quad(&mut mesh, 0);
                    let pixel = 1.0 / self.painter.ctx().pixels_per_point();
                    if ys[0] <= 0.0 {
                        feather_edge(&mut mesh, 0, 2, -pixel);
                    }
                    if ys[1] >= 1.0 {
                        feather_edge(&mut mesh, 1, 3, pixel);
                    }
                    self.painter.add(mesh);
                }
            }
        }
    }
}

/// Print what each half of a new spread holds, once per spread. Opt-in so a
/// normal run leaves stderr empty.
fn log_spread(ctx: &egui::Context, app: &App, spread: Spread) {
    let id = egui::Id::new("book-debug-spread");
    let key = (spread.first(), spread.last());
    let changed = ctx.data_mut(|data| {
        let last = data.get_temp::<(usize, usize)>(id);
        data.insert_temp(id, key);
        last != Some(key)
    });
    if !changed {
        return;
    }
    let sides = spread
        .sides()
        .iter()
        .enumerate()
        .map(|(side, leaf)| match leaf {
            None => format!("side {side} empty"),
            Some(0) => format!(
                "side {side} cover texture={}",
                app.book_texture(0).is_some()
            ),
            Some(leaf) => format!(
                "side {side} page {leaf} texture={}",
                app.book_texture(*leaf).is_some()
            ),
        })
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("book spread {}-{}: {sides}", spread.first(), spread.last());
}

/// The two faces of the sheet a turn folds, or `None` when there is nothing to
/// turn — a wide scan, or a side with no page at all. A lone last page is still
/// a real leaf: it rides the back of the page being flipped.
fn sheet_faces(turn: Turn) -> Option<[Option<usize>; 2]> {
    let outgoing = usize::from(!turn.forward);
    let faces = [turn.from.sides()[outgoing], turn.to.sides()[1 - outgoing]];
    (faces[0].is_some() && faces[1].is_some()).then_some(faces)
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// The largest outward bow of a paper leaf, in points. Used by the page profile
/// and by a reveal clip that must cover the bowed silhouette.
fn page_bow(width: f32) -> f32 {
    (width * 0.0035).clamp(0.6, 1.8)
}

/// The vertical spread of a page's top and bottom edges at `distance` from the
/// binding. The page dips into the gutter, bows gently outward just past it,
/// then lies flat toward the fore-edge. Positive pulls the edges inward.
fn page_offset(distance: f32, width: f32, spine: f32, profile: f32) -> f32 {
    let dip = 3.0 * (1.0 - distance / spine).max(0.0).powi(2);
    let span = width * 0.5;
    let rise = (distance - spine).max(0.0) / (span - spine).max(1.0);
    let bow = page_bow(width);
    let bow = if rise < 1.0 {
        bow * (std::f32::consts::PI * rise).sin().powi(2)
    } else {
        0.0
    };
    (dip - bow) * profile
}

fn spine_alpha(position: f32) -> f32 {
    let stop = position.clamp(0.0, 1.0) * (SPINE_STOPS.len() - 1) as f32;
    let lower = (stop as usize).min(SPINE_STOPS.len() - 2);
    let a = f32::from(SPINE_STOPS[lower]);
    let b = f32::from(SPINE_STOPS[lower + 1]);
    (a + (b - a) * (stop - lower as f32)) / 255.0
}

/// Sheets resting on each side of the spread. Pages are printed front and back,
/// so two pages make one sheet. A stack only means anything while sheets are
/// still ahead: once the facing page's back is the chapter's last page, the
/// next turn opens no new sheet, so both sides show no stack at all.
fn leaves_around(spread: Spread, pages: usize) -> [usize; 2] {
    if let Spread::Wide(index) = spread {
        return [
            index.saturating_sub(1) / 2,
            pages.saturating_sub(index + 1) / 2,
        ];
    }
    let sides = spread.sides();
    let ahead = sides[1].map_or(0, |right| pages.saturating_sub(right + 2) / 2);
    [
        sides[0].map_or(0, |left| {
            if ahead == 0 {
                0
            } else {
                left.saturating_sub(2) / 2
            }
        }),
        ahead,
    ]
}

/// Whether a page has anything to paint. The cover draws its title even before
/// its art arrives; interior pages draw nothing until they decode.
fn paints_leaf(page: Option<usize>, has_texture: bool) -> bool {
    has_texture || page.is_some()
}

/// Extend just the inner edge over the neighbouring leaf to avoid raster cracks.
fn hinge_overlap(x: f32, center: f32, side: f32, overlap: f32) -> f32 {
    if side != 0.0 && (x - center).abs() < 0.01 {
        center - side.signum() * overlap
    } else {
        x
    }
}

/// A one-device-pixel coverage fringe smooths mesh silhouettes without blurring art.
fn feather_edge(mesh: &mut egui::Mesh, first: u32, second: u32, offset: f32) {
    let base = mesh.vertices.len() as u32;
    for index in [first, second] {
        let mut vertex = mesh.vertices[index as usize];
        vertex.pos.y += offset;
        vertex.color = Color32::TRANSPARENT;
        mesh.vertices.push(vertex);
    }
    mesh.add_triangle(first, second, base);
    mesh.add_triangle(base, second, base + 1);
}

fn feather_page_edges(mesh: &mut egui::Mesh, pixel: f32) {
    let columns = mesh.vertices.len() as u32 / 2;
    for column in 1..columns {
        let previous = (column - 1) * 2;
        feather_edge(mesh, previous, previous + 2, -pixel);
        feather_edge(mesh, previous + 1, previous + 3, pixel);
    }
}

fn quad(mesh: &mut egui::Mesh, base: u32) {
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base + 2, base + 1, base + 3);
}

/// A bound sheet whose lower corner leads, returning coordinates in page units.
fn sheet_point(u: f32, v: f32, t: f32, cover: bool, corner: f32) -> (f32, f32, f32) {
    let phase = (t + corner * (v - 0.5) * (std::f32::consts::PI * t).sin()).clamp(0.0, 1.0);
    let (x, z) = fold_point(u, std::f32::consts::PI * phase, cover);
    (x, v - 0.5 - 0.035 * z, z)
}

fn lifted_shadow_alpha(height: f32) -> u8 {
    (17.0 * smooth((height / 0.08).clamp(0.0, 1.0))).round() as u8
}

fn grid_quad(mesh: &mut egui::Mesh, base: u32, stride: u32) {
    mesh.add_triangle(base, base + 1, base + stride);
    mesh.add_triangle(base + stride, base + 1, base + stride + 1);
}

fn fold_point(u: f32, angle: f32, cover: bool) -> (f32, f32) {
    // A simple curved sheet: increase the local angle toward the free edge,
    // without moving the binding or stretching the sheet length.
    let bend = if cover { 0.52 } else { 1.55 } * angle.sin();
    if bend.abs() < 0.001 {
        return (u * angle.cos(), u * angle.sin());
    }
    let edge = angle + bend * (u - 0.44);
    let spine = angle - bend * 0.44;
    (
        (edge.sin() - spine.sin()) / bend,
        (spine.cos() - edge.cos()) / bend,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_keep_rtl_order_around_a_wide_scan_and_an_unpaired_last_page() {
        let at = |index| Spread::at(index, 8, |i| i == 3);
        assert_eq!(at(0).sides(), [Some(0), None]);
        assert_eq!(at(1).sides(), [Some(2), Some(1)]);
        assert_eq!(at(2), at(1));
        assert_eq!(at(3), Spread::Wide(3));
        assert_eq!(at(4).sides(), [Some(4), None]);
        assert_eq!(at(5).sides(), [Some(6), Some(5)]);
        assert_eq!(at(7).sides(), [None, Some(7)]);
        assert_eq!(Spread::at(3, 5, |i| i == 4).sides(), [None, Some(3)]);
    }

    #[test]
    fn spread_labels_read_as_page_ranges() {
        let leaves = |left, right| Spread::Leaves { left, right };
        assert_eq!(leaves(Some(0), None).label(15), "Cover");
        assert_eq!(leaves(Some(14), Some(13)).label(15), "13–14 / 15");
        assert_eq!(leaves(None, Some(15)).label(15), "15 / 15");
    }

    #[test]
    fn the_stack_counts_sheets_not_pages() {
        let leaves = |left, right| Spread::Leaves { left, right };
        // 15 pages: the last one is the lone front of the final sheet.
        assert_eq!(leaves_around(leaves(None, Some(15)), 15), [0, 0]);
        // At 14 | 13 page 15 is the back of the page-14 sheet: the next turn
        // opens no new sheet, so no fake pages are drawn on either side.
        assert_eq!(leaves_around(leaves(Some(14), Some(13)), 15), [0, 0]);
        // One spread earlier the 13/14 and 15/16 sheets are still ahead.
        assert_eq!(leaves_around(leaves(Some(12), Some(11)), 15), [5, 1]);
        // The opening spread has the chapter ahead of it and nothing behind.
        assert_eq!(leaves_around(leaves(Some(2), Some(1)), 15), [0, 6]);
        // The cover has nothing behind it.
        assert_eq!(leaves_around(leaves(Some(0), None), 15), [0, 0]);
    }

    #[test]
    fn unloaded_pages_keep_their_paper_but_empty_sides_do_not() {
        assert!(!paints_leaf(None, false));
        assert!(paints_leaf(Some(3), false));
        assert!(paints_leaf(Some(3), true));
        assert!(paints_leaf(Some(0), false));
        // The last spread holds one page and leaves the other half empty.
        let lone = Spread::at(3, 4, |_| false);
        assert_eq!(lone.sides(), [None, Some(3)]);
    }

    #[test]
    fn a_lone_last_page_rides_the_back_of_the_sheet() {
        let leaves = |left, right| Spread::Leaves { left, right };
        let turn = |from, to, forward| Turn {
            from,
            to,
            forward,
            started: 0.0,
            from_progress: 0.0,
            to_progress: 1.0,
            corner: 0.3,
        };
        // 2 and 1 open; page 3 is the back of the left sheet, which lands right.
        let forward = turn(leaves(Some(2), Some(1)), leaves(None, Some(3)), true);
        assert_eq!(sheet_faces(forward), Some([Some(2), Some(3)]));
        // Coming back the other way shows the same leaf from behind.
        let backward = turn(leaves(None, Some(3)), leaves(Some(2), Some(1)), false);
        assert_eq!(sheet_faces(backward), Some([Some(3), Some(2)]));
        // The cover is the front of the first sheet.
        let cover = turn(leaves(Some(0), None), leaves(Some(2), Some(1)), true);
        assert_eq!(sheet_faces(cover), Some([Some(0), Some(1)]));
        // A wide scan has no sheet, and a side without a page has no face.
        let wide = turn(leaves(Some(2), Some(1)), Spread::Wide(3), true);
        assert_eq!(sheet_faces(wide), None);
        let empty = turn(leaves(Some(2), Some(1)), leaves(None, None), true);
        assert_eq!(sheet_faces(empty), None);
    }

    #[test]
    fn settling_from_a_partial_drag_is_proportionally_shorter() {
        let leaves = |left, right| Spread::Leaves { left, right };
        let turn = |from_progress, to_progress| Turn {
            from: leaves(Some(2), Some(1)),
            to: leaves(Some(4), Some(3)),
            forward: true,
            started: 0.0,
            from_progress,
            to_progress,
            corner: 0.3,
        };
        assert!((turn(0.0, 1.0).duration() - TURN_SECONDS).abs() < 1e-9);
        assert!(turn(0.7, 1.0).duration() < turn(0.0, 1.0).duration());
        // A barely started drag returns in the short floor, not a full turn.
        assert!((turn(0.08, 0.0).duration() - TURN_SECONDS * 0.25).abs() < 1e-9);
    }

    #[test]
    fn the_card_edge_fades_in_and_out_with_the_cover() {
        let leaves = |left, right| Spread::Leaves { left, right };
        let cover = leaves(Some(0), None);
        let turn = |from, to, forward| Turn {
            from,
            to,
            forward,
            started: 0.0,
            from_progress: 0.0,
            to_progress: 1.0,
            corner: 0.3,
        };
        // Opening to a wide scan: full while shut, gone once the reveal moves on.
        let opening = turn(cover, Spread::Wide(1), true);
        assert_eq!(opening.cover_edge_opacity(0.0), Some(1.0));
        assert_eq!(opening.cover_edge_opacity(COVER_EDGE_FADE), Some(0.0));
        // Closing from a wide scan: hidden until the last sliver, full once shut.
        let closing = turn(Spread::Wide(1), cover, false);
        assert_eq!(closing.cover_edge_opacity(0.0), Some(0.0));
        assert_eq!(closing.cover_edge_opacity(1.0 - COVER_EDGE_FADE), Some(0.0));
        assert_eq!(closing.cover_edge_opacity(1.0), Some(1.0));
        // An ordinary page turn never draws the cover's card edge.
        let pages = turn(leaves(Some(2), Some(1)), leaves(Some(4), Some(3)), true);
        assert_eq!(pages.cover_edge_opacity(0.0), None);
    }

    #[test]
    fn a_resting_page_dips_at_the_binding_and_bows_gently() {
        let (width, spine) = (400.0, 38.0);
        let at = |distance| page_offset(distance, width, spine, 1.0);
        // The dip at the binding is unchanged, and the bow starts only past it.
        assert!((at(0.0) - 3.0).abs() < 1e-6);
        assert_eq!(at(spine), 0.0);
        assert!(at(spine + 2.0) < 0.0);
        // The rise is shallow and has flattened again by the outer half.
        assert!(at(width * 0.3) < -1.0);
        assert!(at(width * 0.5).abs() < 1e-6);
        assert_eq!(at(width), 0.0);
        // A wider page keeps the same gentle bow at the same fraction in.
        let wide = width * 2.0;
        assert!(page_offset(wide * 0.3, wide, spine, 1.0) < at(width * 0.3));
    }

    #[test]
    fn a_cover_turn_eases_the_spine_from_flat_to_open() {
        let leaves = |left, right| Spread::Leaves { left, right };
        let cover = leaves(Some(0), None);
        let pages = leaves(Some(2), Some(1));
        let turn = |from, to| Turn {
            from,
            to,
            forward: true,
            started: 0.0,
            from_progress: 0.0,
            to_progress: 1.0,
            corner: 0.3,
        };
        // Opening: card cover at 0, the page on its back fully bowed at 1.
        assert_eq!(turn(cover, pages).spine_profile(0.0), 0.0);
        assert_eq!(turn(cover, pages).spine_profile(1.0), 1.0);
        let dip = |turn: Turn, t| page_offset(0.0, 400.0, 38.0, turn.spine_profile(t));
        assert_eq!(dip(turn(cover, pages), 0.0), 0.0);
        assert!(dip(turn(cover, pages), 0.1) < 0.3);
        assert_eq!(dip(turn(cover, pages), 1.0), 3.0);
        // Closing eases the other way, so both ends meet a resting face.
        assert_eq!(turn(pages, cover).spine_profile(0.0), 1.0);
        assert_eq!(turn(pages, cover).spine_profile(1.0), 0.0);
        assert_eq!(dip(turn(pages, cover), 1.0), 0.0);
        // Ordinary page turns keep the full page bow.
        let interior = turn(pages, leaves(Some(4), Some(3)));
        assert_eq!(interior.spine_profile(0.25), 1.0);
    }

    #[test]
    fn edge_smoothing_preserves_art_and_adds_only_transparent_outer_vertices() {
        let mut mesh = egui::Mesh::default();
        for pos in [
            pos2(0.0, 3.0),
            pos2(0.0, 97.0),
            pos2(5.0, 2.0),
            pos2(5.0, 98.0),
        ] {
            mesh.colored_vertex(pos, Color32::WHITE);
        }
        let original = mesh.vertices.clone();
        feather_page_edges(&mut mesh, 0.5);
        assert_eq!(&mesh.vertices[..4], original.as_slice());
        assert!(mesh.vertices[4..]
            .iter()
            .all(|v| v.color == Color32::TRANSPARENT));
        assert_eq!(mesh.vertices[4].pos.y, 2.5);
        assert_eq!(mesh.vertices[6].pos.y, 97.5);
        assert!(mesh
            .indices
            .iter()
            .all(|i| (*i as usize) < mesh.vertices.len()));
    }

    #[test]
    fn hinge_edges_overlap_without_moving_outer_edges_or_wide_scans() {
        assert_eq!(hinge_overlap(100.0, 100.0, -50.0, 0.375), 100.375);
        assert_eq!(hinge_overlap(100.0, 100.0, 50.0, 0.375), 99.625);
        assert_eq!(hinge_overlap(0.0, 100.0, -50.0, 0.375), 0.0);
        assert_eq!(hinge_overlap(100.0, 100.0, 0.0, 0.375), 100.0);
    }

    #[test]
    fn cast_shadow_is_invisible_at_the_binding_and_at_rest() {
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            for v in [0.0, 0.5, 1.0] {
                assert_eq!(lifted_shadow_alpha(sheet_point(0.0, v, t, false, 0.3).2), 0);
                for endpoint in [0.0, 1.0] {
                    assert_eq!(
                        lifted_shadow_alpha(sheet_point(t, v, endpoint, false, 0.3).2),
                        0
                    );
                }
            }
        }
        assert_eq!(lifted_shadow_alpha(0.001), 0);
        assert!(lifted_shadow_alpha(0.04) < lifted_shadow_alpha(0.08));
    }

    #[test]
    fn shadow_grid_connects_adjacent_columns_and_rows() {
        let mut mesh = egui::Mesh::default();
        grid_quad(&mut mesh, 3, 49);
        assert_eq!(mesh.indices, [3, 4, 52, 52, 4, 53]);
    }

    #[test]
    fn corner_turn_stays_bound_and_flat_at_both_endpoints() {
        for corner in [0.0, 0.3, 0.85] {
            for v in [0.0, 0.5, 1.0] {
                for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
                    let (x, _, z) = sheet_point(0.0, v, t, false, corner);
                    assert!(x.abs() < 1e-6 && z.abs() < 1e-6);
                }
                for t in [0.0, 1.0] {
                    let (x, y, z) = sheet_point(1.0, v, t, false, corner);
                    assert!((x - (1.0 - 2.0 * t)).abs() < 1e-5);
                    assert!((y - (v - 0.5)).abs() < 1e-5 && z.abs() < 1e-5);
                }
            }
        }
        let top = sheet_point(1.0, 0.0, 0.5, false, 0.5);
        let bottom = sheet_point(1.0, 1.0, 0.5, false, 0.5);
        assert!(bottom.0 < top.0);
    }

    #[test]
    fn curling_leaf_stays_bound_and_lands_flat_on_the_other_side() {
        let pi = std::f32::consts::PI;
        for angle in [0.0, pi * 0.25, pi * 0.5, pi * 0.75, pi] {
            assert_eq!(fold_point(0.0, angle, false), (0.0, 0.0));
        }
        assert_eq!(fold_point(1.0, 0.0, false), (1.0, 0.0));
        let (x, z) = fold_point(1.0, pi, false);
        assert!((x + 1.0).abs() < 0.001 && z.abs() < 0.001);
        assert!(fold_point(1.0, pi * 0.5, false).1 > 0.8);
    }
}
