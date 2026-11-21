//! Right-to-left spreads and their paper rendering, independent of scroll layout.

use eframe::egui::{self, pos2, vec2, Color32, Painter, Pos2, Rect, TextureHandle, Vec2};

use crate::{App, ImageSlot, MUTED};

const TURN_SECONDS: f64 = 0.48;
const WIDE_TURN_SECONDS: f64 = 0.28;
const LOOKAHEAD_PAGES: usize = 8;
const PREFETCH_WORKERS: usize = 6;
const UV: Rect = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
const PAPER: Color32 = Color32::from_rgb(248, 246, 240);
const SPINE_STOPS: [u8; 5] = [135, 78, 34, 10, 0];
const SWIPE_IDLE: f64 = 0.18;
const SWIPE_DISTANCE: f32 = 45.0;

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
}

#[derive(Clone, Copy)]
pub(crate) struct Turn {
    from: Spread,
    to: Spread,
    forward: bool,
    started: f64,
    from_progress: f32,
    to_progress: f32,
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
        let spread_at = |index| {
            Spread::at(index, len, |page| {
                page != 0
                    && self.book_texture(page).is_some_and(|texture| {
                        let size = texture.size_vec2();
                        size.x >= size.y * 1.15
                    })
            })
        };
        let spread = spread_at(current);
        let previous = spread.first().checked_sub(1).map(spread_at);
        let next = (spread.last() + 1 < len).then(|| spread_at(spread.last() + 1));
        let (area, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let controls = Rect::from_min_max(
            pos2(area.left() + 16.0, area.bottom() - 40.0),
            pos2(area.right() - 16.0, area.bottom() - 8.0),
        );
        let painter = ui.painter().clone();
        let center = pos2(area.center().x, (area.top() + controls.top()) * 0.5);
        let height = (area.width() * 0.5 / 0.7 - 32.0)
            .min(controls.top() - area.top() - 24.0)
            .max(1.0)
            * self.reader_settings.scale;
        let book = Book {
            painter: &painter,
            center,
            leaf: vec2(height * 0.7, height),
            progress: spread.first() as f32 / len.saturating_sub(1).max(1) as f32,
        };

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
                            },
                            0.0,
                        ));
                    }
                }
                if let Some((turn, progress)) = &mut self.book_drag {
                    if turn.forward == forward && (response.dragged() || response.drag_stopped()) {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                        // Use the total distance from the grab, not the last frame's delta.
                        let dx = ctx.input(|input| {
                            match (input.pointer.press_origin(), input.pointer.interact_pos()) {
                                (Some(start), Some(pointer)) => pointer.x - start.x,
                                _ => 0.0,
                            }
                        });
                        if response.dragged() || dx != 0.0 {
                            *progress = (dx * if forward { 1.0 } else { -1.0 }
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
        if let Some(turn) = self.book_turn {
            let t = ((now - turn.started) / turn.duration()).clamp(0.0, 1.0) as f32;
            book.turn(
                self,
                turn,
                egui::lerp(turn.from_progress..=turn.to_progress, smooth(t)),
            );
            ctx.request_repaint();
        } else if let Some((turn, progress)) = self.book_drag {
            book.turn(self, turn, progress);
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
                    ImageSlot::Ready(_) => {}
                    _ => {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            format!("Loading page {}…", index + 1),
                            egui::FontId::proportional(13.0),
                            Color32::from_gray(84),
                        );
                    }
                }
            }
        }

        let mut forward = false;
        let mut backward = false;
        let can_turn = self.book_turn.is_none()
            && self.book_drag.is_none()
            && self.book_pending_turn.is_none();
        ui.scope_builder(egui::UiBuilder::new().max_rect(controls), |ui| {
            ui.horizontal(|ui| {
                forward = ui
                    .add_enabled(
                        can_turn && next.is_some(),
                        egui::Button::new("← Next spread"),
                    )
                    .clicked();
                let count = len - 1;
                let label = if self.book_pending_turn.is_some() {
                    "Loading spread…".to_owned()
                } else {
                    match (spread.first(), spread.last()) {
                        (0, _) => "Cover".to_owned(),
                        (first, last) if first == last => format!("{first} / {count}"),
                        (first, last) => format!("{first}–{last} / {count}"),
                    }
                };
                ui.label(egui::RichText::new(label).color(MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    backward = ui
                        .add_enabled(
                            can_turn && previous.is_some(),
                            egui::Button::new("Previous spread →"),
                        )
                        .clicked();
                });
            });
        });
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
    progress: f32,
}

impl Book<'_> {
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
        let progress = if side == 0 {
            1.0 - self.progress
        } else {
            self.progress
        };
        let depth = (3.0 + 8.0 * progress) * (self.leaf.x / 260.0).min(1.0) * amount;
        let lip = depth + 2.0 * amount;
        let cover = rect.translate(vec2(outward * lip, lip));
        let shadow = cover
            .translate(vec2(0.0, 3.0 * amount))
            .expand(2.0 * amount);
        // The stack is only an edge. Painting its interior exposes a blank
        // duplicate leaf as soon as the curling artwork lifts away.
        let (outer_min, outer_max) = if side == 0 {
            (shadow.left(), rect.left())
        } else {
            (rect.right(), shadow.right())
        };
        for edge in [
            Rect::from_min_max(pos2(outer_min, rect.top()), pos2(outer_max, rect.bottom())),
            Rect::from_min_max(pos2(shadow.left(), rect.bottom()), shadow.max),
        ] {
            let painter = painter.with_clip_rect(edge);
            painter.rect_filled(shadow, 4.0, Color32::from_black_alpha(110));
            painter.rect_filled(cover, 2.0, Color32::from_rgb(62, 48, 42));
            for layer in (1..=7).rev() {
                let offset = depth * layer as f32 / 7.0;
                painter.rect_filled(
                    rect.translate(vec2(outward * offset, offset)),
                    1.0,
                    Color32::from_gray(if layer % 2 == 0 { 190 } else { 225 }),
                );
            }
        }
    }

    fn spread(&self, app: &App, spread: Spread) {
        match spread {
            Spread::Wide(index) => {
                self.stack(0, self.side(0), 1.0);
                self.stack(1, self.side(1), 1.0);
                self.flat(app, self.page_rect(spread, index), Some(index));
                self.spine(0);
                self.spine(1);
            }
            Spread::Leaves { left, right } => {
                for (side, page) in [left, right].into_iter().enumerate() {
                    if page.is_some() {
                        let rect = self.side(side);
                        if left.is_some() && right.is_some() {
                            self.stack(side, rect, 1.0);
                        }
                        self.flat(app, rect, page);
                        if page != Some(0) {
                            self.spine(side);
                        }
                    }
                }
            }
        }
    }

    fn flat(&self, app: &App, rect: Rect, page: Option<usize>) {
        self.painter.rect_filled(rect, 0.0, PAPER);
        if let Some(texture) = page.and_then(|index| app.book_texture(index)) {
            let art = fitted_art(rect.size(), texture.size_vec2());
            let art = Rect::from_center_size(rect.center(), art);
            if page == Some(0) {
                self.painter.image(texture.id(), art, UV, Color32::WHITE);
            } else {
                // Include the exact gutter column so a wide image stays one
                // continuous texture without a seam or a duplicated strip.
                let mut columns = (0..=32)
                    .map(|i| egui::lerp(art.x_range(), i as f32 / 32.0))
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
                            pos: pos2(x, y + self.gutter_offset(x)),
                            uv: pos2((x - art.left()) / art.width(), v),
                            color: Color32::WHITE,
                        });
                    }
                    if i > 0 {
                        quad(&mut mesh, (i as u32 - 1) * 2);
                    }
                }
                self.painter.with_clip_rect(rect).add(mesh);
            }
        } else if page == Some(0) {
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
    }

    fn gutter_offset(&self, x: f32) -> f32 {
        let distance = (x - self.center.x).abs() / self.spine_width();
        3.0 * (1.0 - distance).max(0.0).powi(2)
    }

    fn spine(&self, side: usize) {
        self.shadow(side, self.spine_width(), &SPINE_STOPS, self.side(side));
    }

    fn spine_width(&self) -> f32 {
        (self.leaf.x * 0.12).min(38.0)
    }

    fn shadow(&self, side: usize, width: f32, stops: &[u8], rect: Rect) {
        let direction = if side == 0 { -1.0 } else { 1.0 };
        let mut mesh = egui::Mesh::default();
        for (i, &alpha) in stops.iter().enumerate() {
            let x = self.center.x + direction * width * i as f32 / (stops.len() - 1) as f32;
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(pos2(x, rect.top()), Color32::from_black_alpha(alpha));
            mesh.colored_vertex(pos2(x, rect.bottom()), Color32::from_black_alpha(alpha));
            if i > 0 {
                quad(&mut mesh, base - 2);
            }
        }
        self.painter.add(mesh);
    }

    fn turn(&self, app: &App, turn: Turn, t: f32) {
        let page_count = app.pages.len().max(1) as f32;
        let progress = |spread: Spread| spread.first() as f32 / page_count;
        let (Spread::Leaves { .. }, Spread::Leaves { .. }) = (turn.from, turn.to) else {
            // A full-width scan has no turnable leaf. Fade through the
            // background rather than showing two layouts over each other.
            let (spread, opacity) = if t < 0.5 {
                (turn.from, 1.0 - 2.0 * t)
            } else {
                (turn.to, 2.0 * t - 1.0)
            };
            let mut painter = self.painter.clone();
            painter.set_opacity(opacity);
            Book {
                painter: &painter,
                progress: progress(spread),
                ..*self
            }
            .spread(app, spread);
            return;
        };
        let outgoing = usize::from(!turn.forward);
        let incoming = 1 - outgoing;
        let old = turn.from.sides();
        let new = turn.to.sides();
        let stack = Book {
            progress: egui::lerp(progress(turn.from)..=progress(turn.to), t),
            ..*self
        };
        let cover_turn = old.contains(&Some(0)) || new.contains(&Some(0));
        let old_stack = old.iter().all(Option::is_some);
        let new_stack = new.iter().all(Option::is_some);
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
            stack.stack(side, rect, amount);
            let Some(page) = page else { continue };
            self.flat(app, rect, Some(page));
            if page != 0 {
                self.spine(side);
            }
            self.cast_shadow(side, outgoing, rect, t, cover_turn);
        }
        self.curl(app, [old[outgoing], new[incoming]], outgoing, t, cover_turn);
    }

    fn cast_shadow(&self, side: usize, outgoing: usize, rect: Rect, t: f32, cover: bool) {
        let angle = std::f32::consts::PI * t;
        let (x, lift) = fold_point(0.85, angle, cover);
        let direction = if outgoing == 0 { -1.0 } else { 1.0 };
        let width = self.leaf.x * (0.025 + 0.23 * lift);
        let center = self.center.x + direction * self.leaf.x * x + width * 0.45;
        let alpha = (85.0 * lift.clamp(0.0, 1.0)) as u8;
        let mut mesh = egui::Mesh::default();
        for (i, (offset, opacity)) in [
            (-1.0, 0),
            (-0.4, alpha / 2),
            (0.0, alpha),
            (0.55, alpha / 3),
            (1.25, 0),
        ]
        .into_iter()
        .enumerate()
        {
            let x = center + width * offset;
            let color = Color32::from_black_alpha(opacity);
            mesh.colored_vertex(pos2(x, rect.top()), color);
            mesh.colored_vertex(pos2(x, rect.bottom()), color);
            if i > 0 {
                quad(&mut mesh, (i as u32 - 1) * 2);
            }
        }
        self.painter.with_clip_rect(self.side(side)).add(mesh);
    }

    fn curl(&self, app: &App, pages: [Option<usize>; 2], outgoing: usize, t: f32, cover: bool) {
        const STRIPS: usize = 36;
        let direction = if outgoing == 0 { -1.0 } else { 1.0 };
        let angle = std::f32::consts::PI * t;
        let columns: Vec<_> = (0..=STRIPS)
            .map(|index| {
                let u = index as f32 / STRIPS as f32;
                let (x, z) = fold_point(u, angle, cover);
                (u, x, z)
            })
            .collect();
        let mut strips: Vec<_> = columns.windows(2).collect();
        strips.sort_by(|a, b| (a[0].2 + a[1].2).total_cmp(&(b[0].2 + b[1].2)));
        for strip in strips {
            let reverse = strip[1].1 < strip[0].1;
            let Some(page) = pages[usize::from(reverse)] else {
                // An unpaired leaf has no sheet to paint on this face.
                continue;
            };
            let texture = app.book_texture(page);
            let face_direction = if reverse { -direction } else { direction };
            let art = texture.map_or(self.leaf, |tex| fitted_art(self.leaf, tex.size_vec2()));
            let art_rect = Rect::from_center_size(pos2(0.5, 0.5), art / self.leaf);
            // Split at the art bounds so the paper margins bend with the image.
            let mut cuts = vec![strip[0].0, strip[1].0];
            for edge in [art_rect.left(), art_rect.right()] {
                if edge > strip[0].0 && edge < strip[1].0 {
                    cuts.insert(cuts.len() - 1, edge);
                }
            }
            for segment in cuts.windows(2) {
                let in_art = art_rect.x_range().contains((segment[0] + segment[1]) * 0.5);
                for (top, bottom, textured) in [
                    (0.0, art_rect.top(), false),
                    (art_rect.top(), art_rect.bottom(), in_art),
                    (art_rect.bottom(), 1.0, false),
                ] {
                    if bottom <= top {
                        continue;
                    }
                    let texture = if textured { texture } else { None };
                    let mut mesh = egui::Mesh::with_texture(
                        texture.map_or(egui::TextureId::default(), TextureHandle::id),
                    );
                    for u in [segment[0], segment[1]] {
                        let (x, z) = fold_point(u, angle, cover);
                        let perspective = 1.0 + 0.08 * z;
                        let crease = if page == 0 {
                            0.0
                        } else {
                            spine_alpha(u * self.leaf.x / self.spine_width())
                        };
                        // Match the resting page's crease at both ends of the turn.
                        let brightness = (1.0 - crease) * (1.0 - 0.16 * z.abs());
                        let base = if texture.is_some() {
                            Color32::WHITE
                        } else {
                            PAPER
                        };
                        let shade = |channel: u8| (f32::from(channel) * brightness).round() as u8;
                        let color =
                            Color32::from_rgb(shade(base.r()), shade(base.g()), shade(base.b()));
                        for v in [top, bottom] {
                            let art_x = if face_direction > 0.0 { u } else { 1.0 - u };
                            let uv = if texture.is_some() {
                                pos2(
                                    (art_x - art_rect.left()) / art_rect.width(),
                                    (v - art_rect.top()) / art_rect.height(),
                                )
                            } else {
                                egui::epaint::WHITE_UV
                            };
                            let mut pos = self.center
                                + vec2(
                                    direction * x * self.leaf.x * perspective,
                                    ((v - 0.5) * perspective - 0.045 * z) * self.leaf.y,
                                );
                            if texture.is_some() && page != 0 {
                                pos.y += self.gutter_offset(pos.x) * (1.0 - angle.sin());
                            }
                            mesh.vertices.push(egui::epaint::Vertex { pos, uv, color });
                        }
                    }
                    quad(&mut mesh, 0);
                    self.painter.add(mesh);
                }
            }
        }
    }
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn spine_alpha(position: f32) -> f32 {
    let stop = position.clamp(0.0, 1.0) * (SPINE_STOPS.len() - 1) as f32;
    let lower = (stop as usize).min(SPINE_STOPS.len() - 2);
    let a = f32::from(SPINE_STOPS[lower]);
    let b = f32::from(SPINE_STOPS[lower + 1]);
    (a + (b - a) * (stop - lower as f32)) / 255.0
}

fn fitted_art(paper: Vec2, image: Vec2) -> Vec2 {
    image * (paper.x / image.x).min(paper.y / image.y)
}

fn quad(mesh: &mut egui::Mesh, base: u32) {
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base + 2, base + 1, base + 3);
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
    fn settling_from_a_partial_drag_is_proportionally_shorter() {
        let leaves = |left, right| Spread::Leaves { left, right };
        let turn = |from_progress, to_progress| Turn {
            from: leaves(Some(2), Some(1)),
            to: leaves(Some(4), Some(3)),
            forward: true,
            started: 0.0,
            from_progress,
            to_progress,
        };
        assert!((turn(0.0, 1.0).duration() - TURN_SECONDS).abs() < 1e-9);
        assert!(turn(0.7, 1.0).duration() < turn(0.0, 1.0).duration());
        // A barely started drag returns in the short floor, not a full turn.
        assert!((turn(0.08, 0.0).duration() - TURN_SECONDS * 0.25).abs() < 1e-9);
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
