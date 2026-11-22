use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{RenderableCursor, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use eframe::egui::{self, Color32, FontId, Galley, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2};

use super::Events;
use crate::theme::{ACCENT, CANVAS};

const ANSI: [(u8, u8, u8); 16] = [
    (22, 26, 34),
    (239, 122, 128),
    (137, 205, 148),
    (226, 189, 116),
    (123, 168, 249),
    (197, 151, 233),
    (107, 202, 201),
    (191, 200, 213),
    (100, 113, 134),
    (255, 151, 156),
    (159, 229, 170),
    (249, 214, 148),
    (155, 191, 255),
    (220, 181, 255),
    (142, 227, 226),
    (237, 242, 250),
];
const SELECTION: Color32 = Color32::from_rgb(49, 61, 58);
const MAX_CACHED_GLYPHS: usize = 1024;

pub(super) fn default_color(index: usize) -> Rgb {
    let (r, g, b) = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let cube = index - 16;
            let scale = |part| if part == 0 { 0 } else { (55 + 40 * part) as u8 };
            (scale(cube / 36), scale(cube / 6 % 6), scale(cube % 6))
        }
        232..=255 => {
            let shade = (8 + 10 * (index - 232)) as u8;
            (shade, shade, shade)
        }
        257 => (CANVAS.r(), CANVAS.g(), CANVAS.b()),
        258 => (ACCENT.r(), ACCENT.g(), ACCENT.b()),
        259..=266 => {
            let (r, g, b) = ANSI[index - 259];
            (r / 2 + r / 8, g / 2 + g / 8, b / 2 + b / 8)
        }
        267 => ANSI[15],
        268 => (133, 146, 164),
        _ => (221, 221, 221),
    };
    Rgb { r, g, b }
}

fn resolve_color(color: Color, colors: &Colors) -> Color32 {
    let indexed = |index: usize| colors[index].unwrap_or_else(|| default_color(index));
    let rgb = match color {
        Color::Spec(rgb) => rgb,
        Color::Named(named) => indexed(named as usize),
        Color::Indexed(index) => indexed(index.into()),
    };
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

struct VisibleCell {
    row: usize,
    col: usize,
    cell: Cell,
    foreground: Color32,
    background: Color32,
}

pub(super) struct Screen {
    cells: Vec<VisibleCell>,
    cursor: Option<(usize, usize, CursorShape, bool)>,
    cursor_color: Color32,
    background: Color32,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            cursor: None,
            cursor_color: Color32::WHITE,
            background: CANVAS,
        }
    }
}

impl Screen {
    pub fn capture(&mut self, term: &Term<Events>) {
        let view = term.renderable_content();
        let RenderableCursor { point, shape } = view.cursor;
        let cursor_row = point.line.0 + view.display_offset as i32;
        self.cursor = (shape != CursorShape::Hidden
            && (0..term.grid().screen_lines() as i32).contains(&cursor_row))
        .then(|| {
            let wide = term.grid()[point].flags.contains(Flags::WIDE_CHAR);
            (cursor_row as usize, point.column.0, shape, wide)
        });
        self.cursor_color = resolve_color(Color::Named(NamedColor::Cursor), view.colors);
        self.background = resolve_color(Color::Named(NamedColor::Background), view.colors);
        self.cells.clear();

        for item in view.display_iter {
            let Point { line, column } = item.point;
            let cell = item.cell;
            let flags = cell.flags;
            let selected = view
                .selection
                .is_some_and(|range| range.contains(item.point));
            if cell.c == ' '
                && cell.bg == Color::Named(NamedColor::Background)
                && !selected
                && !flags.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
            {
                continue;
            }

            let fg = match cell.fg {
                Color::Named(color) if flags.contains(Flags::DIM) => Color::Named(color.to_dim()),
                Color::Named(color) if flags.contains(Flags::BOLD) => {
                    Color::Named(color.to_bright())
                }
                color => color,
            };
            let mut foreground = resolve_color(fg, view.colors);
            let mut background = resolve_color(cell.bg, view.colors);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut foreground, &mut background);
            }
            if selected {
                background = SELECTION;
                foreground = Color32::WHITE;
            }
            self.cells.push(VisibleCell {
                row: (line.0 + view.display_offset as i32) as usize,
                col: column.0,
                cell: cell.clone(),
                foreground,
                background,
            });
        }
    }
}

#[derive(Default)]
pub struct Renderer {
    glyphs: HashMap<char, Arc<Galley>>,
    pixels_per_point: f32,
    cell: Vec2,
}

impl Renderer {
    fn font() -> FontId {
        FontId::monospace(14.0)
    }

    pub(super) fn measure(&mut self, ui: &egui::Ui) -> Vec2 {
        let scale = ui.ctx().pixels_per_point();
        if self.pixels_per_point != scale {
            self.pixels_per_point = scale;
            self.glyphs.clear();
            self.cell = ui.fonts_mut(|fonts| {
                let font = Self::font();
                let width = fonts.glyph_width(&font, 'M');
                let height = fonts.row_height(&font) + 2.0;
                egui::vec2(
                    (width * scale).round() / scale,
                    (height * scale).ceil() / scale,
                )
            });
        }
        self.cell
    }

    pub(super) fn paint(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        size: Vec2,
        screen: &Screen,
        focused: bool,
    ) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, screen.background);
        let origin = rect.min;
        let position =
            |row: usize, col: usize| origin + egui::vec2(col as f32 * size.x, row as f32 * size.y);

        let mut cells = screen.cells.iter().peekable();
        while let Some(first) = cells.next() {
            if first.background == screen.background {
                continue;
            }
            let mut end = first.col + 1;
            while cells
                .next_if(|next| {
                    next.row == first.row && next.col == end && next.background == first.background
                })
                .is_some()
            {
                end += 1;
            }
            let pos = position(first.row, first.col);
            painter.rect_filled(
                Rect::from_min_max(pos, position(first.row + 1, end)),
                0.0,
                first.background,
            );
        }

        for item in &screen.cells {
            let flags = item.cell.flags;
            if flags.intersects(
                Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            let pos = position(item.row, item.col);
            let width = if flags.contains(Flags::WIDE_CHAR) {
                size.x * 2.0
            } else {
                size.x
            };
            if item.cell.c != ' ' {
                self.glyph(&painter, pos, item);
            }
            if flags.intersects(Flags::ALL_UNDERLINES) {
                let start = pos + egui::vec2(0.0, size.y - 2.0);
                painter.hline(
                    start.x..=start.x + width,
                    start.y,
                    Stroke::new(1.0, item.foreground),
                );
            }
            if flags.contains(Flags::STRIKEOUT) {
                painter.hline(
                    pos.x..=pos.x + width,
                    pos.y + size.y * 0.5,
                    Stroke::new(1.0, item.foreground),
                );
            }
        }

        if let Some((row, col, shape, wide)) = screen.cursor {
            let pos = position(row, col);
            let width = if wide { size.x * 2.0 } else { size.x };
            let bounds = Rect::from_min_size(pos, egui::vec2(width, size.y));
            let color = screen.cursor_color;
            match if focused {
                shape
            } else {
                CursorShape::HollowBlock
            } {
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(pos, egui::vec2(1.5, size.y)),
                        0.0,
                        color,
                    );
                }
                CursorShape::Underline => {
                    painter.hline(
                        pos.x..=bounds.max.x,
                        bounds.max.y - 1.0,
                        Stroke::new(1.5, color),
                    );
                }
                CursorShape::Block => {
                    painter.rect_filled(bounds, 0.0, color.gamma_multiply(0.38));
                    painter.rect_stroke(bounds, 0.0, Stroke::new(1.0, color), StrokeKind::Inside);
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(bounds, 0.0, Stroke::new(1.0, color), StrokeKind::Inside);
                }
                CursorShape::Hidden => {}
            }
        }
    }

    fn glyph(&mut self, painter: &Painter, pos: Pos2, item: &VisibleCell) {
        let c = item.cell.c;
        let galley = if let Some(marks) = item.cell.zerowidth() {
            let text = std::iter::once(c).chain(marks.iter().copied()).collect();
            painter.layout_no_wrap(text, Self::font(), Color32::PLACEHOLDER)
        } else if let Some(galley) = self.glyphs.get(&c) {
            galley.clone()
        } else {
            let galley = painter.layout_no_wrap(c.to_string(), Self::font(), Color32::PLACEHOLDER);
            if self.glyphs.len() < MAX_CACHED_GLYPHS {
                self.glyphs.insert(c, galley.clone());
            }
            galley
        };
        let y = pos.y + ((self.cell.y - galley.size().y) * 0.5).max(0.0);
        painter.galley(egui::pos2(pos.x, y), galley, item.foreground);
    }
}
