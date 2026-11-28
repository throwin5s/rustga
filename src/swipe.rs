//! The swipe stack: one scanned series at a time, drag right to keep it, drag
//! left to pass. The buttons and arrow keys do the same thing, because a trackpad
//! drag is not the only way people will want to answer.

use eframe::egui::{self, Color32, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};

use crate::{App, CardState, FAINT, MUTED, SURFACE};

/// How far a card travels sideways before letting go counts as a choice.
const THRESHOLD: f32 = 110.0;
const CARD_RADIUS: f32 = 14.0;
const ADD: Color32 = Color32::from_rgb(104, 176, 124);
const SKIP: Color32 = Color32::from_rgb(200, 108, 108);

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Action {
    Add,
    Skip,
}

pub(crate) fn stack(app: &mut App, ui: &mut egui::Ui, ctx: &egui::Context) -> Option<Action> {
    let card = app.scan_cards.get(app.scan_index)?.clone();
    let area = ui.available_rect_before_wrap();
    ui.allocate_exact_size(area.size(), Sense::hover());

    let width = (area.width() * 0.40).clamp(300.0, 390.0);
    let height = (area.height() - 84.0).clamp(300.0, 600.0);
    let center = egui::pos2(area.center().x, area.top() + height * 0.5 + 6.0);
    let resting = Rect::from_center_size(center, Vec2::new(width, height));

    // Two cards behind the top one: enough to read as a stack, no more.
    for (step, inset) in [(2.0, 18.0), (1.0, 9.0)] {
        let behind = Rect::from_center_size(
            center + Vec2::new(0.0, step * 5.0),
            Vec2::new(width - inset * 2.0, height),
        );
        ui.painter()
            .rect_filled(behind, CARD_RADIUS, Color32::from_gray(26));
    }

    let response = ui.interact(
        resting,
        ui.id().with(("scan-card", app.scan_index)),
        Sense::drag(),
    );
    if response.dragged() {
        app.swipe_drag += response.drag_delta().x;
    } else if !response.drag_stopped() && app.swipe_drag != 0.0 {
        // Let a card that was not thrown far enough spring back.
        app.swipe_drag *= 0.55;
        if app.swipe_drag.abs() < 0.5 {
            app.swipe_drag = 0.0;
        } else {
            ctx.request_repaint();
        }
    }

    let drag = app.swipe_drag;
    let mut action = None;
    if response.drag_stopped() {
        if drag >= THRESHOLD {
            action = Some(Action::Add);
        } else if drag <= -THRESHOLD {
            action = Some(Action::Skip);
        }
        app.swipe_drag = 0.0;
    }

    let moved = resting.translate(Vec2::new(drag, 0.0));
    ui.painter().rect_filled(moved, CARD_RADIUS, SURFACE);

    let ratio = (drag.abs() / THRESHOLD).clamp(0.0, 1.0);
    if ratio > 0.01 {
        let color = if drag > 0.0 { ADD } else { SKIP };
        ui.painter()
            .rect_filled(moved, CARD_RADIUS, color.gamma_multiply(0.18 * ratio));
        ui.painter().rect_stroke(
            moved,
            CARD_RADIUS,
            Stroke::new(2.0_f32, color.gamma_multiply(ratio)),
            StrokeKind::Inside,
        );
        let (label, position, align) = if drag > 0.0 {
            (
                "ADD",
                moved.right_top() + Vec2::new(-16.0, 16.0),
                egui::Align2::RIGHT_TOP,
            )
        } else {
            (
                "SKIP",
                moved.left_top() + Vec2::new(16.0, 16.0),
                egui::Align2::LEFT_TOP,
            )
        };
        ui.painter().text(
            position,
            align,
            label,
            egui::FontId::proportional(13.0),
            color,
        );
    }

    let provider = match &card.state {
        CardState::Found { source, .. } => app
            .source_options
            .iter()
            .find(|option| option.info().id == *source)
            .map(|option| option.info().name.clone()),
        CardState::Missing => None,
    };
    let in_library = match &card.state {
        CardState::Found { source, manga } => app.library.contains(*source, &manga.url),
        CardState::Missing => false,
    };
    let (title, cover, author, genres, synopsis, meta, alt_titles, note) = match &card.state {
        CardState::Found { manga, .. } => {
            let mut meta = Vec::new();
            if let Some(demographic) = manga
                .demographic
                .as_deref()
                .filter(|demographic| !demographic.trim().is_empty())
            {
                meta.push(demographic.to_owned());
            }
            if let Some(years) = manga.years() {
                meta.push(years);
            }
            if let Some(status) = manga.status.label() {
                meta.push(status.to_owned());
            }
            if let Some(provider) = provider.clone() {
                meta.push(provider);
            }
            let others = manga
                .alt_titles
                .iter()
                .filter(|alt| !alt.trim().is_empty())
                .filter(|alt| !alt.eq_ignore_ascii_case(&manga.title))
                .take(2)
                .cloned()
                .collect::<Vec<_>>();
            (
                manga.title.clone(),
                manga.thumbnail_url.clone(),
                manga.author.clone(),
                manga.genre.clone(),
                manga.description.clone(),
                meta.join("  ·  "),
                others,
                None,
            )
        }
        CardState::Missing => (
            card.rec.title.clone(),
            None,
            None,
            None,
            None,
            String::new(),
            Vec::new(),
            Some("Not found in your sources. Swipe right to search for it by hand."),
        ),
    };

    let inner = moved.shrink(14.0);
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.set_clip_rect(moved);
        let cover_height = (inner.height() * 0.42).clamp(120.0, 240.0);
        let (cover_rect, _) =
            ui.allocate_exact_size(Vec2::new(inner.width(), cover_height), Sense::hover());
        if let Some(url) = cover.as_deref() {
            app.cover(ui, cover_rect, Some(url), 0.0, ctx);
        } else {
            ui.painter()
                .rect_filled(cover_rect, 8.0, Color32::from_gray(38));
            ui.painter().text(
                cover_rect.center(),
                egui::Align2::CENTER_CENTER,
                trim(&title, 60),
                egui::FontId::proportional(15.0),
                MUTED,
            );
        }
        ui.add_space(12.0);
        ui.add(egui::Label::new(RichText::new(&title).size(17.0).strong()).truncate());
        if in_library {
            ui.add_space(2.0);
            ui.add(egui::Label::new(
                RichText::new("In your library").size(10.5).color(ADD),
            ));
        }
        if let Some(author) = author.as_deref().filter(|author| !author.trim().is_empty()) {
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(author).size(11.5).color(MUTED)).truncate());
        }
        if let Some(genres) = genres.as_deref().filter(|genres| !genres.trim().is_empty()) {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(genres).size(10.5).color(FAINT)).truncate());
        }
        if !meta.is_empty() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(&meta).size(10.5).color(MUTED)).truncate());
        }
        if !alt_titles.is_empty() {
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Also: {}", alt_titles.join(" · ")))
                        .size(10.0)
                        .color(FAINT),
                )
                .truncate(),
            );
        }
        ui.add_space(8.0);
        // The whole synopsis, in its own scroll area: the wheel reads it and a
        // drag still swipes, so a long description never hides the ending.
        let footer = 84.0;
        let body = (ui.available_height() - footer).max(90.0);
        egui::ScrollArea::vertical()
            .id_salt(("scan-synopsis", app.scan_index))
            .max_height(body)
            .scroll_source(egui::scroll_area::ScrollSource {
                drag: false,
                ..Default::default()
            })
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(description) = synopsis
                    .as_deref()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                {
                    ui.add(
                        egui::Label::new(
                            RichText::new(description)
                                .size(11.5)
                                .color(Color32::from_gray(176)),
                        )
                        .wrap(),
                    );
                } else if let Some(note) = note {
                    ui.add(egui::Label::new(RichText::new(note).size(11.5).color(FAINT)).wrap());
                }
            });

        // Pinned to the bottom: the comment the recommendation came from.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            let quote = card
                .rec
                .quote
                .as_deref()
                .map(str::trim)
                .filter(|quote| !quote.is_empty());
            // The layout is bottom-up, so the last thing added sits highest.
            if let Some(why) = card
                .rec
                .why
                .as_deref()
                .map(str::trim)
                .filter(|why| !why.is_empty())
            {
                ui.add(
                    egui::Label::new(
                        RichText::new(trim(why, 110))
                            .size(10.5)
                            .color(Color32::from_gray(152)),
                    )
                    .wrap(),
                );
            }
            // Without a quote the heading would label empty space, and a title
            // the patterns pulled out of a line has none.
            if let Some(quote) = quote {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("“{}”", trim(quote, 190)))
                            .size(11.0)
                            .italics()
                            .color(Color32::from_gray(168)),
                    )
                    .wrap(),
                );
                ui.label(RichText::new("FROM THE THREAD").size(9.5).color(FAINT));
            }
        });
    });

    let gap = 12.0;
    let button = Vec2::new(140.0, 34.0);
    let row = resting.bottom() + 24.0;
    let skip_rect = Rect::from_center_size(egui::pos2(center.x - 70.0 - gap, row), button);
    let add_rect = Rect::from_center_size(egui::pos2(center.x + 70.0 + gap, row), button);
    if ui
        .put(
            skip_rect,
            egui::Button::new(RichText::new("Skip").size(13.0)),
        )
        .clicked()
    {
        action = Some(Action::Skip);
    }
    let add_label = if in_library {
        "In library"
    } else {
        "Add to library"
    };
    if ui
        .put(
            add_rect,
            egui::Button::new(RichText::new(add_label).size(13.0)),
        )
        .clicked()
        && !in_library
    {
        action = Some(Action::Add);
    }

    let (left, right) = ui.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowLeft),
            input.key_pressed(egui::Key::ArrowRight),
        )
    });
    if action.is_none() {
        if right {
            action = Some(Action::Add);
        } else if left {
            action = Some(Action::Skip);
        }
    }
    action
}

fn trim(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let mut cut = text.chars().take(max.saturating_sub(1)).collect::<String>();
    cut.push('…');
    cut
}
