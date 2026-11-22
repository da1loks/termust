mod input;
mod paint;

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Osc52, Term};
use alacritty_terminal::{tty, tty::Shell};
use anyhow::{Context, Result};
use eframe::egui::{self, Response, Sense};

use crate::connection::Launch;

pub use paint::Renderer;

const SCROLLBACK_LINES: usize = 4_000;
const INITIAL_SIZE: Size = Size(WindowSize {
    num_lines: 24,
    num_cols: 80,
    cell_width: 8,
    cell_height: 18,
});

#[derive(Clone, Copy)]
struct Size(WindowSize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.0.num_lines.into()
    }

    fn columns(&self) -> usize {
        self.0.num_cols.into()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(Option<ExitStatus>),
}

#[derive(Clone)]
struct Events(Arc<Shared>);

struct Shared {
    id: u64,
    active: Arc<AtomicU64>,
    context: egui::Context,
    dirty: AtomicBool,
    size: Mutex<WindowSize>,
    status: Mutex<Status>,
    sender: OnceLock<EventLoopSender>,
}

impl Events {
    fn send(&self, message: Msg) {
        if let Some(sender) = self.0.sender.get() {
            let _ = sender.send(message);
        }
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            self.send(Msg::Input(bytes));
        }
    }

    fn redraw(&self) {
        let already_dirty = self.0.dirty.swap(true, Ordering::AcqRel);
        if !already_dirty && self.0.active.load(Ordering::Relaxed) == self.0.id {
            self.0.context.request_repaint();
        }
    }
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.redraw(),
            Event::PtyWrite(text) => self.write(text.into_bytes()),
            Event::TextAreaSizeRequest(reply) => {
                self.write(reply(*self.0.size.lock().unwrap()).into_bytes());
            }
            Event::ColorRequest(index, reply) => {
                self.write(reply(paint::default_color(index)).into_bytes());
            }
            Event::ChildExit(status) => {
                *self.0.status.lock().unwrap() = Status::Exited(Some(status));
                self.0.context.request_repaint();
            }
            Event::Exit => {
                let mut status = self.0.status.lock().unwrap();
                if *status == Status::Running {
                    *status = Status::Exited(None);
                }
                self.0.context.request_repaint();
            }
            _ => {}
        }
    }
}

pub struct Session {
    term: Arc<FairMutex<Term<Events>>>,
    events: Events,
    screen: paint::Screen,
    input: input::State,
}

impl Session {
    pub fn open(
        id: u64,
        launch: &Launch,
        active: Arc<AtomicU64>,
        context: egui::Context,
    ) -> Result<Self> {
        let program = launch
            .program
            .to_str()
            .context("Неверный путь к программе")?;
        // Alacritty escapes the arguments on Windows. The executable path is passed verbatim.
        let mut environment = std::collections::HashMap::from([
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ]);
        if let Some(askpass) = &launch.askpass {
            environment.extend(askpass.environment()?);
        }
        let options = tty::Options {
            shell: Some(Shell::new(format!("\"{program}\""), launch.args.clone())),
            escape_args: true,
            drain_on_exit: true,
            working_directory: std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .filter(|path| path.is_dir()),
            env: environment,
        };
        let pty = tty::new(&options, INITIAL_SIZE.0, id)
            .with_context(|| format!("Не удалось запустить {}", launch.program.display()))?;
        let events = Events(Arc::new(Shared {
            id,
            active,
            context,
            dirty: AtomicBool::new(true),
            size: Mutex::new(INITIAL_SIZE.0),
            status: Mutex::new(Status::Running),
            sender: OnceLock::new(),
        }));
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &INITIAL_SIZE,
            events.clone(),
        )));
        let event_loop = EventLoop::new(
            term.clone(),
            events.clone(),
            pty,
            options.drain_on_exit,
            false,
        )
        .context("Не удалось запустить обработчик терминала")?;
        events.0.sender.set(event_loop.channel()).ok();
        event_loop.spawn();
        Ok(Self {
            term,
            events,
            screen: paint::Screen::default(),
            input: input::State::default(),
        })
    }

    pub fn status(&self) -> Status {
        *self.events.0.status.lock().unwrap()
    }

    pub fn dimensions(&self) -> (u16, u16) {
        let size = self.events.0.size.lock().unwrap();
        (size.num_cols, size.num_lines)
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        renderer: &mut Renderer,
        request_focus: bool,
        enabled: bool,
    ) -> Response {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let cell = renderer.measure(ui);
        self.resize(Size(WindowSize {
            num_cols: ((rect.width() / cell.x).floor() as u16).clamp(2, 500),
            num_lines: ((rect.height() / cell.y).floor() as u16).clamp(2, 300),
            cell_width: (cell.x * ui.ctx().pixels_per_point()).round() as u16,
            cell_height: (cell.y * ui.ctx().pixels_per_point()).round() as u16,
        }));

        let paste = enabled && response.secondary_clicked();
        if enabled && (request_focus || response.clicked() || response.drag_started() || paste) {
            response.request_focus();
        }
        if paste {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        let focused = enabled && response.has_focus() && ui.input(|input| input.focused);
        if enabled {
            input::handle(self, &response, rect, cell, focused);
        }
        if self.events.0.dirty.swap(false, Ordering::AcqRel) {
            self.screen.capture(&self.term.lock());
        }
        renderer.paint(
            ui,
            rect,
            cell,
            &self.screen,
            focused && self.status() == Status::Running,
        );
        response
    }

    fn resize(&mut self, size: Size) {
        let mut current = self.events.0.size.lock().unwrap();
        if (
            current.num_cols,
            current.num_lines,
            current.cell_width,
            current.cell_height,
        ) == (
            size.0.num_cols,
            size.0.num_lines,
            size.0.cell_width,
            size.0.cell_height,
        ) {
            return;
        }
        *current = size.0;
        drop(current);
        self.term.lock().resize(size);
        self.events.send(Msg::Resize(size.0));
        self.events.redraw();
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        use alacritty_terminal::grid::Scroll;

        if self.status() == Status::Running {
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
                self.events.redraw();
            }
            drop(term);
            self.events.write(bytes);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.events.send(Msg::Shutdown);
    }
}
