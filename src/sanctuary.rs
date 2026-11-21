//! A quiet, paint-only backdrop for the reader, independent of its layout.

use eframe::egui::{self, vec2, Color32, Painter, Pos2, Shape, Stroke, Vec2};
use std::f32::consts::TAU;
use std::time::Duration;

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Mode {
    #[default]
    Off,
    Still,
    Animated,
}

impl Mode {
    pub(crate) const OPTIONS: [(Self, &'static str); 3] = [
        (Self::Off, "Off"),
        (Self::Still, "Still"),
        (Self::Animated, "Animated"),
    ];
}

pub(crate) fn paint(ui: &egui::Ui, mode: Mode, daylight: bool) {
    let rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    if mode == Mode::Off || !rect.is_positive() {
        return;
    }
    let time = if mode == Mode::Animated {
        if ui.input(|input| input.focused) {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        ui.input(|input| input.time)
    } else {
        0.0
    };
    let painter = ui.painter().with_clip_rect(rect);
    let at = |x, y| rect.min + vec2(rect.width() * x, rect.height() * y);
    let unit = rect.size().min_elem();

    let rgb = |[r, g, b]: [u8; 3]| Color32::from_rgb(r, g, b);
    let sky_colors = if daylight {
        [[121, 156, 174], [204, 221, 215], [225, 223, 195]]
    } else {
        [[30, 36, 56], [91, 76, 102], [128, 87, 105]]
    };
    let mut sky = egui::Mesh::default();
    for (y, color) in [0.0, 0.65, 1.0].into_iter().zip(sky_colors.map(rgb)) {
        sky.colored_vertex(at(0.0, y), color);
        sky.colored_vertex(at(1.0, y), color);
    }
    for i in [0, 2] {
        sky.add_triangle(i, i + 1, i + 2);
        sky.add_triangle(i + 1, i + 3, i + 2);
    }
    painter.add(sky);
    let light = if daylight {
        [255, 240, 188]
    } else {
        [250, 184, 191]
    };
    let [r, g, b] = light;
    wash(
        &painter,
        at(0.70, 0.40),
        unit * 0.56,
        Color32::from_rgba_unmultiplied(r, g, b, if daylight { 112 } else { 70 }),
    );
    let hills = if daylight {
        [[143, 173, 169], [113, 150, 145], [82, 126, 117]]
    } else {
        [[96, 87, 111], [70, 73, 95], [52, 61, 77]]
    };
    // Distant ridges fade into mist rather than ending in a hard band.
    for (layer, [r, g, b]) in hills.into_iter().enumerate() {
        let layer = layer as f32;
        let mut ridge = egui::Mesh::default();
        for i in 0..=96 {
            let x = i as f32 / 96.0;
            let y = 0.54
                + layer * 0.14
                + (x * 7.8 + layer * 1.8).sin() * 0.071
                + (x * 17.0 - layer * 1.3).cos() * 0.033
                + (x * 36.0 + layer).sin() * 0.010;
            for (y, alpha) in [(y - 0.011, 0), (y + 0.014, 204), (1.0, 245)] {
                ridge.colored_vertex(at(x, y), Color32::from_rgba_unmultiplied(r, g, b, alpha));
            }
            if i > 0 {
                for j in [(i - 1) * 3, (i - 1) * 3 + 1] {
                    ridge.add_triangle(j, j + 1, j + 3);
                    ridge.add_triangle(j + 1, j + 4, j + 3);
                }
            }
        }
        painter.add(ridge);
    }

    let ink = rgb(if daylight { [88, 65, 72] } else { [39, 30, 46] });
    for (curve, width) in [
        (
            [
                at(-0.04, 0.33),
                at(0.06, 0.26),
                at(0.04, 0.11),
                at(0.31, 0.035),
            ],
            0.019,
        ),
        (
            [
                at(0.053, 0.21),
                at(0.12, 0.21),
                at(0.15, 0.14),
                at(0.24, 0.17),
            ],
            0.008,
        ),
        (
            [
                at(0.09, 0.15),
                at(0.13, 0.10),
                at(0.14, 0.034),
                at(0.16, -0.018),
            ],
            0.006,
        ),
        (
            [
                at(1.03, 0.21),
                at(0.94, 0.21),
                at(0.95, 0.095),
                at(0.77, 0.04),
            ],
            0.012,
        ),
        (
            [
                at(0.94, 0.142),
                at(0.90, 0.14),
                at(0.86, 0.17),
                at(0.81, 0.147),
            ],
            0.006,
        ),
    ] {
        let mut mesh = egui::Mesh::default();
        for i in 0..=24 {
            let t = i as f32 / 24.0;
            let a = curve[0]
                .lerp(curve[1], t)
                .lerp(curve[1].lerp(curve[2], t), t);
            let b = curve[1]
                .lerp(curve[2], t)
                .lerp(curve[2].lerp(curve[3], t), t);
            let side = (b - a).normalized().rot90() * (unit * width * 0.5 * (1.0 - t));
            let p = a.lerp(b, t);
            mesh.colored_vertex(p - side, ink);
            mesh.colored_vertex(p + side, ink);
            if i > 0 {
                let j = (i - 1) * 2;
                mesh.add_triangle(j, j + 1, j + 2);
                mesh.add_triangle(j + 1, j + 3, j + 2);
            }
        }
        painter.add(mesh);
    }

    for (cluster, (x, y)) in [
        (0.027, 0.26),
        (0.083, 0.18),
        (0.138, 0.123),
        (0.158, 0.055),
        (0.23, 0.165),
        (0.253, 0.063),
        (0.302, 0.033),
        (0.974, 0.189),
        (0.91, 0.119),
        (0.856, 0.152),
        (0.793, 0.047),
    ]
    .into_iter()
    .enumerate()
    {
        for i in 0..6 {
            let n = cluster as u32 * 6 + i;
            let center =
                at(x, y) + unit * vec2((seed(n + 4) - 0.5) * 0.083, (seed(n + 82) - 0.5) * 0.065);
            let radius = unit * (0.006 + 0.016 * seed(n + 156));
            let color = Color32::from_rgba_unmultiplied(
                222,
                148 + (seed(n + 29) * 39.0) as u8,
                173,
                67 + (seed(n + 56) * 79.0) as u8,
            );
            let phase = seed(n + 53) * TAU;
            for j in 0..5 {
                let a = phase + j as f32 * TAU / 5.0;
                painter.circle_filled(
                    center + vec2(a.cos(), a.sin()) * radius * 0.56,
                    radius * 0.46,
                    color,
                );
            }
            painter.circle_filled(
                center,
                radius * 0.23,
                Color32::from_rgba_unmultiplied(239, 190, 160, 145),
            );
        }
    }

    for i in 0..18 {
        let depth = seed(i + 32);
        let speed = 0.029 + f64::from(depth) * 0.036;
        let y = (f64::from(seed(i + 60)) + time * speed).rem_euclid(1.12) as f32 - 0.06;
        let sway = (time * 0.52).rem_euclid(f64::from(TAU)) as f32 + seed(i + 31) * TAU;
        let x = seed(i) + sway.sin() * 0.035 + y * 0.10;
        let roll = (time * (0.85 + f64::from(depth) * 0.9)).rem_euclid(f64::from(TAU)) as f32
            + seed(i + 145) * TAU;
        let angle = sway.sin() * 0.7 + y * 1.2;
        let size = (unit * (0.007 + 0.010 * depth)).clamp(2.8, 11.0);
        let axis = vec2(angle.sin(), angle.cos()) * size;
        let flank = axis.rot90() * (0.22 + 0.78 * roll.cos().abs());
        petal(&painter, at(x, y), axis, flank, depth);
    }
}

fn petal(painter: &Painter, center: Pos2, axis: Vec2, flank: Vec2, depth: f32) {
    let at = |x, y| center + flank * x + axis * y;
    let alpha = (92.0 + 74.0 * depth) as u8;
    // Rounded, asymmetric silhouette; a translucent center stroke suggests the fold.
    painter.add(Shape::convex_polygon(
        [
            (-0.04, 0.60),
            (-0.25, 0.38),
            (-0.42, 0.09),
            (-0.49, -0.17),
            (-0.40, -0.40),
            (-0.19, -0.54),
            (0.10, -0.58),
            (0.34, -0.48),
            (0.46, -0.28),
            (0.45, -0.05),
            (0.36, 0.18),
            (0.19, 0.42),
        ]
        .into_iter()
        .map(|(x, y)| at(x, y))
        .collect(),
        Color32::from_rgba_unmultiplied(231, 156, 179, alpha),
        Stroke::NONE,
    ));
    if flank.length() > 3.0 {
        painter.add(Shape::line(
            vec![
                at(-0.035, 0.39),
                at(-0.11, 0.12),
                at(-0.135, -0.14),
                at(-0.09, -0.38),
            ],
            Stroke::new(
                0.65_f32,
                Color32::from_rgba_unmultiplied(251, 204, 212, alpha / 2),
            ),
        ));
    }
}

fn wash(painter: &Painter, center: Pos2, radius: f32, color: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, color);
    for i in 0..32 {
        let angle = i as f32 * TAU / 32.0;
        let ray = vec2(angle.cos(), angle.sin()) * radius;
        mesh.colored_vertex(center + ray * 0.46, color.gamma_multiply(0.55));
        mesh.colored_vertex(center + ray, Color32::TRANSPARENT);
        let current = 1 + i * 2;
        let next = 1 + (i + 1) % 32 * 2;
        mesh.add_triangle(0, current, next);
        mesh.add_triangle(current, current + 1, next);
        mesh.add_triangle(current + 1, next + 1, next);
    }
    painter.add(mesh);
}

fn seed(i: u32) -> f32 {
    (i.wrapping_mul(747_796_405)
        .rotate_left(13)
        .wrapping_mul(2_891_336_453)
        >> 16) as f32
        / 65_536.0
}
