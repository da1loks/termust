use std::borrow::Cow;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
use eframe::egui::{
    Event, ImeEvent, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, Rect, Response, Vec2,
};

use super::Session;

#[derive(Default)]
pub(super) struct State {
    selecting: bool,
    scroll: f32,
}

pub(super) fn handle(
    session: &mut Session,
    response: &Response,
    rect: Rect,
    cell: Vec2,
    focused: bool,
) {
    let context = &response.ctx;
    let (events, modifiers) = context.input(|input| (input.events.clone(), input.modifiers));
    if events.is_empty() {
        return;
    }
    let mode = *session.term.lock().mode();

    for event in events {
        match event {
            Event::Text(text) if focused => {
                let mut bytes = text.into_bytes();
                if modifiers.alt && !modifiers.ctrl {
                    bytes.insert(0, 0x1b);
                }
                session.write(bytes);
            }
            Event::Ime(ImeEvent::Commit(text)) if focused => session.write(text.into_bytes()),
            Event::Paste(text) if focused => session.write(paste(&text, mode)),
            Event::Copy if focused && modifiers.shift => copy(session, response),
            Event::Copy if focused => session.write(b"\x03".as_slice()),
            Event::Cut if focused => session.write(b"\x18".as_slice()),
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if focused => {
                if modifiers.shift && matches!(key, Key::PageUp | Key::PageDown) {
                    let scroll = if key == Key::PageUp {
                        Scroll::PageUp
                    } else {
                        Scroll::PageDown
                    };
                    session.term.lock().scroll_display(scroll);
                    session.events.redraw();
                } else if let Some(bytes) = keystroke(key, modifiers, mode) {
                    session.write(bytes);
                }
            }
            Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } if response.hovered() => {
                let distance = match unit {
                    MouseWheelUnit::Point => delta.y / cell.y,
                    MouseWheelUnit::Line => delta.y * 3.0,
                    MouseWheelUnit::Page => delta.y * (rect.height() / cell.y),
                };
                session.input.scroll += distance;
                let lines = session.input.scroll.trunc() as i32;
                session.input.scroll -= lines as f32;
                if lines == 0 {
                    continue;
                }
                if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                    && !modifiers.shift
                {
                    let sequence = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
                    session.write(sequence.repeat(lines.unsigned_abs().min(100) as usize));
                } else {
                    session.term.lock().scroll_display(Scroll::Delta(lines));
                    session.events.redraw();
                }
            }
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                ..
            } if response.contains_pointer() && rect.contains(pos) => {
                session.input.selecting = true;
                let mut term = session.term.lock();
                let (point, side) = selection_point(term.grid(), pos, rect, cell);
                term.selection = Some(Selection::new(SelectionType::Simple, point, side));
                drop(term);
                session.events.redraw();
            }
            Event::PointerMoved(pos) if session.input.selecting => {
                let mut term = session.term.lock();
                let (point, side) = selection_point(term.grid(), pos, rect, cell);
                if let Some(selection) = term.selection.as_mut() {
                    selection.update(point, side);
                }
                drop(term);
                session.events.redraw();
            }
            Event::PointerButton {
                button: PointerButton::Primary,
                pressed: false,
                ..
            }
            | Event::PointerGone => session.input.selecting = false,
            _ => {}
        }
    }
}

fn copy(session: &Session, response: &Response) {
    let selection = session.term.lock().selection_to_string();
    if let Some(text) = selection {
        response.ctx.copy_text(text);
    }
}

fn selection_point(
    grid: &alacritty_terminal::Grid<alacritty_terminal::term::cell::Cell>,
    pos: Pos2,
    rect: Rect,
    size: Vec2,
) -> (Point, Side) {
    let x = (pos.x - rect.min.x) / size.x;
    let y = (pos.y - rect.min.y) / size.y;
    let col = (x as usize).min(grid.columns() - 1);
    let row = (y as usize).min(grid.screen_lines() - 1);
    let side = if x - (col as f32) < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let point = viewport_to_point(grid.display_offset(), Point::new(row, Column(col)));
    (point, side)
}

fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
    let mut clean = String::with_capacity(text.len() + if bracketed { 12 } else { 0 });
    if bracketed {
        clean.push_str("\x1b[200~");
    }
    for c in text.replace("\r\n", "\n").chars() {
        match c {
            '\n' if !bracketed => clean.push('\r'),
            '\n' | '\r' | '\t' => clean.push(c),
            _ if !c.is_control() => clean.push(c),
            _ => {}
        }
    }
    if bracketed {
        clean.push_str("\x1b[201~");
    }
    clean.into_bytes()
}

fn keystroke(key: Key, modifiers: Modifiers, mode: TermMode) -> Option<Cow<'static, [u8]>> {
    let modifier =
        1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);

    let cursor = match key {
        Key::ArrowUp => 'A',
        Key::ArrowDown => 'B',
        Key::ArrowRight => 'C',
        Key::ArrowLeft => 'D',
        Key::Home => 'H',
        Key::End => 'F',
        _ => '\0',
    };
    if cursor != '\0' {
        let seq = if modifier > 1 {
            format!("\x1b[1;{modifier}{cursor}")
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{cursor}")
        } else {
            format!("\x1b[{cursor}")
        };
        return Some(Cow::Owned(seq.into_bytes()));
    }

    let function = match key {
        Key::F1 => 'P',
        Key::F2 => 'Q',
        Key::F3 => 'R',
        Key::F4 => 'S',
        _ => '\0',
    };
    if function != '\0' {
        let seq = if modifier > 1 {
            format!("\x1b[1;{modifier}{function}")
        } else {
            format!("\x1bO{function}")
        };
        return Some(Cow::Owned(seq.into_bytes()));
    }

    let code = match key {
        Key::Insert => 2,
        Key::Delete => 3,
        Key::PageUp => 5,
        Key::PageDown => 6,
        Key::F5 => 15,
        Key::F6 => 17,
        Key::F7 => 18,
        Key::F8 => 19,
        Key::F9 => 20,
        Key::F10 => 21,
        Key::F11 => 23,
        Key::F12 => 24,
        _ => 0,
    };
    if code > 0 {
        let seq = if modifier > 1 {
            format!("\x1b[{code};{modifier}~")
        } else {
            format!("\x1b[{code}~")
        };
        return Some(Cow::Owned(seq.into_bytes()));
    }

    if key == Key::Tab && modifiers.shift {
        return Some(Cow::Borrowed(b"\x1b[Z"));
    }
    let byte = match key {
        Key::Enter => b'\r',
        Key::Tab => b'\t',
        Key::Escape => 0x1b,
        Key::Backspace if modifiers.ctrl => 0x08,
        Key::Backspace => 0x7f,
        _ if modifiers.ctrl && !modifiers.alt => match (key, key.name().as_bytes()) {
            (Key::Space | Key::Num2, _) => 0,
            (Key::OpenBracket, _) => 0x1b,
            (Key::Backslash, _) => 0x1c,
            (Key::CloseBracket, _) => 0x1d,
            (Key::Num6, _) => 0x1e,
            (Key::Minus, _) => 0x1f,
            (_, [c @ b'A'..=b'Z']) => *c & 0x1f,
            _ => return None,
        },
        _ => return None,
    };
    Some(Cow::Owned(if modifiers.alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }))
}
