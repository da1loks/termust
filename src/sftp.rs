mod client;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{
    self, Button, DragAndDrop, Id, Key, Pos2, RichText, ScrollArea, Sense, TextEdit, Ui,
};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::connection::Launch;
use crate::theme::{ACCENT, BORDER, ERROR, MUTED};

use client::{Entry, Kind, Listing, Request, Update, Worker, remote_child};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Connected,
    Closed,
}

#[derive(Clone, Copy)]
enum Direction {
    Download,
    Upload,
}

enum Busy {
    Remote,
    Local,
    Transfer {
        direction: Direction,
        name: String,
        total: Option<u64>,
    },
}

enum Navigation<P> {
    EnterAddress,
    Open(P),
}

struct FileDrag<P> {
    tab: u64,
    file: Entry<P>,
}

struct PaneActions<P> {
    navigation: Option<Navigation<P>>,
    destination: Option<P>,
}

struct Pane<P> {
    listing: Option<Listing<P>>,
    address: String,
    selected: Option<usize>,
}

impl<P> Default for Pane<P> {
    fn default() -> Self {
        Self {
            listing: None,
            address: String::new(),
            selected: None,
        }
    }
}

impl<P> Pane<P> {
    fn receive(
        &mut self,
        result: Result<Listing<P>, String>,
        display: impl FnOnce(&P) -> String,
    ) -> Option<String> {
        match result {
            Ok(listing) => {
                self.address = display(&listing.path);
                self.listing = Some(listing);
                self.selected = None;
                None
            }
            Err(error) => {
                if let Some(listing) = &self.listing {
                    self.address = display(&listing.path);
                }
                Some(error)
            }
        }
    }

    fn file(&self) -> Option<&Entry<P>> {
        self.listing
            .as_ref()?
            .entries
            .get(self.selected?)
            .filter(|entry| entry.kind == Kind::File)
    }
}

pub struct SftpView {
    id: u64,
    worker: Worker,
    remote: Pane<String>,
    local: Pane<PathBuf>,
    phase: Phase,
    busy: Option<Busy>,
    message: Option<(String, bool)>,
}

impl SftpView {
    pub fn open(
        launch: Launch,
        id: u64,
        active: Arc<AtomicU64>,
        context: egui::Context,
    ) -> Result<Self> {
        Ok(Self {
            id,
            worker: Worker::spawn(launch, id, active, context)?,
            remote: Pane::default(),
            local: Pane::default(),
            phase: Phase::Connecting,
            busy: None,
            message: None,
        })
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn update(&mut self) {
        while let Ok(update) = self.worker.updates.try_recv() {
            match update {
                Update::Connected(result) => {
                    self.phase = Phase::Connected;
                    if let Some(error) = self.remote.receive(result, Clone::clone) {
                        self.message = Some((error, true));
                    }
                }
                Update::Remote(result) => {
                    self.busy = None;
                    if let Some(error) = self.remote.receive(result, Clone::clone) {
                        self.message = Some((error, true));
                    }
                }
                Update::Local(result) => {
                    if matches!(self.busy, Some(Busy::Local)) {
                        self.busy = None;
                    }
                    if let Some(error) = self.local.receive(result, |path| display_local(path)) {
                        self.message = Some((error, true));
                    }
                }
                Update::Transferred(report) => {
                    let Some(Busy::Transfer {
                        direction, name, ..
                    }) = self.busy.take()
                    else {
                        continue;
                    };
                    if report.completed > 0 {
                        match direction {
                            Direction::Download => {
                                if let Some(listing) = &self.local.listing {
                                    self.request(Request::Local(listing.path.clone()), Busy::Local);
                                }
                            }
                            Direction::Upload => {
                                if let Some(listing) = &self.remote.listing {
                                    self.request(
                                        Request::Remote(listing.path.clone()),
                                        Busy::Remote,
                                    );
                                }
                            }
                        }
                    }
                    if self.phase != Phase::Closed {
                        self.message = Some(match report.first_error {
                            Some(error) if report.completed > 0 => (
                                format!("Передано файлов · {}\n{error}", report.completed),
                                true,
                            ),
                            Some(error) => (error, true),
                            None if report.completed > 1 => {
                                (format!("Передано файлов · {}", report.completed), false)
                            }
                            None => (format!("Передано · {name}"), false),
                        });
                    }
                }
                Update::Disconnected(error) => {
                    self.phase = Phase::Closed;
                    self.busy = None;
                    self.message = Some((error, true));
                }
            }
        }
    }

    fn request(&mut self, request: Request, busy: Busy) {
        self.message = None;
        if self.worker.requests.try_send(request).is_ok() {
            self.busy = Some(busy);
        } else {
            self.phase = Phase::Closed;
            self.busy = None;
            self.message = Some(("Сеанс SFTP завершён".into(), true));
        }
    }

    pub fn show(&mut self, ui: &mut Ui) {
        let can_browse = ui.is_enabled() && self.phase == Phase::Connected && self.busy.is_none();
        let remote_path = self
            .remote
            .listing
            .as_ref()
            .filter(|listing| self.remote.address == listing.path)
            .map(|listing| listing.path.as_str());
        let local_path = self
            .local
            .listing
            .as_ref()
            .filter(|listing| self.local.address == display_local(&listing.path))
            .map(|listing| listing.path.as_path());
        let ready = can_browse && remote_path.is_some() && local_path.is_some();
        let allow_explorer_drop = can_browse && remote_path.is_some();

        let (download, upload) = ui
            .horizontal(|ui| {
                let download = ui
                    .add_enabled(
                        ready && self.remote.file().is_some(),
                        Button::new("Скачать →").frame(false),
                    )
                    .clicked();
                let upload = ui
                    .add_enabled(
                        ready && self.local.file().is_some(),
                        Button::new("← Загрузить").frame(false),
                    )
                    .clicked();
                (download, upload)
            })
            .inner;
        if download && let (Some(file), Some(directory)) = (self.remote.file(), local_path) {
            self.download(file.clone(), directory.to_path_buf());
        } else if upload && let (Some(file), Some(directory)) = (self.local.file(), remote_path) {
            self.upload(vec![file.path.clone()], directory.to_owned(), file.size);
        }
        if let Some(Busy::Transfer { name, total, .. }) = &self.busy {
            let done = self.worker.bytes.load(Ordering::Relaxed);
            let text = format!("{name} · {}", size_label(done));
            if let Some(total) = total.filter(|total| *total > 0) {
                let fraction = (done as f64 / total as f64).min(1.0) as f32;
                ui.add(egui::ProgressBar::new(fraction).text(text).fill(ACCENT));
            } else {
                ui.label(RichText::new(text).color(MUTED));
            }
        } else if let Some((message, error)) = &self.message {
            let tint = if *error { ERROR } else { MUTED };
            ui.label(RichText::new(message).color(tint));
        }
        ui.separator();

        let can_browse = can_browse && self.busy.is_none();
        let ready = ready && can_browse;
        let allow_explorer_drop = allow_explorer_drop && can_browse;
        let remote_drag = ready
            .then(|| DragAndDrop::payload::<FileDrag<String>>(ui.ctx()))
            .flatten()
            .filter(|drag| drag.tab == self.id);
        let local_drag = ready
            .then(|| DragAndDrop::payload::<FileDrag<PathBuf>>(ui.ctx()))
            .flatten()
            .filter(|drag| drag.tab == self.id);
        let (pointer, released, explorer_hover, explorer_drop) = ui.input(|input| {
            (
                input.pointer.interact_pos(),
                input.pointer.button_released(egui::PointerButton::Primary),
                !input.raw.hovered_files.is_empty(),
                !input.raw.dropped_files.is_empty(),
            )
        });
        let explorer_at = if allow_explorer_drop && (explorer_hover || explorer_drop) {
            if explorer_hover {
                ui.ctx().request_repaint_after(Duration::from_millis(32));
            }
            explorer_drop_point(ui)
        } else {
            None
        };
        let painter = ui.painter().clone();
        let drag_tab = ready.then_some(self.id);
        let (left, right) = ui.columns_const(|[left, right]| {
            let bounds = left.max_rect();
            let separator = (bounds.right() + right.max_rect().left()) * 0.5;
            painter.vline(separator, bounds.y_range(), egui::Stroke::new(1.0, BORDER));
            let left = pane_ui(
                left,
                "remote",
                "Сервер",
                &mut self.remote,
                can_browse,
                drag_tab,
                explorer_at.or_else(|| local_drag.as_ref().and(pointer)),
            );
            let right = pane_ui(
                right,
                "local",
                "Этот ПК",
                &mut self.local,
                can_browse,
                drag_tab,
                if remote_drag.is_some() { pointer } else { None },
            );
            (left, right)
        });
        if remote_drag.is_some() || local_drag.is_some() {
            let valid = left.destination.is_some() || right.destination.is_some();
            ui.ctx().set_cursor_icon(if valid {
                egui::CursorIcon::Copy
            } else {
                egui::CursorIcon::NoDrop
            });
        }
        if let Some(directory) = left.destination {
            if !explorer_drop
                && released
                && let Some(drag) = local_drag
            {
                DragAndDrop::clear_payload(ui.ctx());
                self.upload(vec![drag.file.path.clone()], directory, drag.file.size);
                return;
            }
            if explorer_drop && explorer_at.is_some() {
                let paths = ui.input_mut(|input| {
                    std::mem::take(&mut input.raw.dropped_files)
                        .into_iter()
                        .filter_map(|file| file.path)
                        .collect::<Vec<_>>()
                });
                if !paths.is_empty() {
                    self.upload(paths, directory, None);
                    return;
                }
            }
        }
        if let (true, Some(directory), Some(drag)) = (released, right.destination, remote_drag) {
            DragAndDrop::clear_payload(ui.ctx());
            self.download(drag.file.clone(), directory);
            return;
        }
        if let Some(action) = left.navigation {
            let path = match action {
                Navigation::Open(path) => path,
                Navigation::EnterAddress => {
                    let address = self.remote.address.trim();
                    if address.starts_with('/') {
                        address.into()
                    } else {
                        self.remote.listing.as_ref().map_or_else(
                            || address.into(),
                            |listing| remote_child(&listing.path, address),
                        )
                    }
                }
            };
            self.request(Request::Remote(path), Busy::Remote);
        } else if let Some(action) = right.navigation {
            let path = match action {
                Navigation::Open(path) => path,
                Navigation::EnterAddress => {
                    let address = PathBuf::from(self.local.address.trim());
                    if address.is_absolute() {
                        address
                    } else if let Some(listing) = &self.local.listing {
                        listing.path.join(address)
                    } else {
                        address
                    }
                }
            };
            self.request(Request::Local(path), Busy::Local);
        }
    }

    fn download(&mut self, file: Entry<String>, directory: PathBuf) {
        self.worker.bytes.store(0, Ordering::Relaxed);
        self.request(
            Request::Download {
                source: file.path,
                directory,
                name: file.name.clone(),
            },
            Busy::Transfer {
                direction: Direction::Download,
                name: file.name,
                total: file.size,
            },
        );
    }

    fn upload(&mut self, sources: Vec<PathBuf>, directory: String, total: Option<u64>) {
        let name = if let [source] = sources.as_slice() {
            source
                .file_name()
                .unwrap_or(source.as_os_str())
                .to_string_lossy()
                .into_owned()
        } else {
            format!("Загрузка файлов ({})", sources.len())
        };
        self.worker.bytes.store(0, Ordering::Relaxed);
        self.request(
            Request::Upload { sources, directory },
            Busy::Transfer {
                direction: Direction::Upload,
                name,
                total,
            },
        );
    }
}

fn pane_ui<P: Clone + Send + Sync + 'static>(
    ui: &mut Ui,
    id: &'static str,
    title: &str,
    pane: &mut Pane<P>,
    enabled: bool,
    drag_tab: Option<u64>,
    drop_at: Option<Pos2>,
) -> PaneActions<P> {
    const ROW: f32 = 23.0;
    ui.push_id(id, |ui| {
        let bounds = ui.max_rect().intersect(ui.clip_rect());
        let drop_at = drop_at.filter(|pos| bounds.contains(*pos));
        let mut navigation = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let listing = pane.listing.as_ref();
                if ui
                    .add_enabled(enabled && listing.is_some(), Button::new("↻").frame(false))
                    .on_hover_text("Обновить")
                    .clicked()
                    && let Some(listing) = listing
                {
                    navigation = Some(Navigation::Open(listing.path.clone()));
                }
                let parent = listing.and_then(|listing| listing.parent.as_ref());
                if ui
                    .add_enabled(enabled && parent.is_some(), Button::new("↑").frame(false))
                    .on_hover_text("На уровень выше")
                    .clicked()
                    && let Some(parent) = parent
                {
                    navigation = Some(Navigation::Open(parent.clone()));
                }
            });
        });
        let address = ui.add_enabled(
            enabled,
            TextEdit::singleline(&mut pane.address)
                .id(Id::new((ui.id(), "address")))
                .desired_width(f32::INFINITY)
                .hint_text("Путь"),
        );
        if address.lost_focus()
            && ui.input(|input| input.key_pressed(Key::Enter))
            && !pane.address.trim().is_empty()
        {
            navigation = Some(Navigation::EnterAddress);
        }
        ui.add_space(4.0);
        let Some(listing) = &pane.listing else {
            return PaneActions {
                navigation,
                destination: None,
            };
        };
        let mut target = drop_at.map(|_| (&listing.path, bounds.shrink(1.0)));
        if listing.entries.is_empty() {
            ui.label(RichText::new("Папка пуста").color(MUTED));
        }
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW, listing.entries.len(), |ui, rows| {
                let width = ui.available_width();
                for index in rows {
                    let entry = &listing.entries[index];
                    let name = entry.name.replace(char::is_control, "�");
                    let name = RichText::new(name);
                    let name = if pane.selected == Some(index) {
                        name.color(ACCENT)
                    } else {
                        name
                    };
                    let detail = match entry.kind {
                        Kind::Directory => "Папка".into(),
                        Kind::Link => "Ссылка".into(),
                        Kind::File => entry.size.map_or_else(String::new, size_label),
                        Kind::Other => String::new(),
                    };
                    let row = Button::new(name)
                        .frame(false)
                        .truncate()
                        .right_text(RichText::new(detail).size(11.0).color(MUTED))
                        .min_size(egui::vec2(width, ROW));
                    let row = if drag_tab.is_some() && entry.kind == Kind::File {
                        row.sense(Sense::click_and_drag())
                    } else {
                        row
                    };
                    let response = ui.add_enabled(enabled, row);
                    let started_drag = entry.kind == Kind::File
                        && response.drag_started_by(egui::PointerButton::Primary);
                    if response.clicked() || started_drag {
                        pane.selected = Some(index);
                    }
                    if started_drag && let Some(tab) = drag_tab {
                        DragAndDrop::set_payload(
                            ui.ctx(),
                            FileDrag {
                                tab,
                                file: entry.clone(),
                            },
                        );
                    }
                    if response.double_clicked()
                        && matches!(entry.kind, Kind::Directory | Kind::Link)
                    {
                        navigation = Some(Navigation::Open(entry.path.clone()));
                    }
                    if entry.kind == Kind::Directory
                        && drop_at.is_some_and(|pos| response.interact_rect.contains(pos))
                    {
                        target = Some((&entry.path, response.interact_rect));
                    }
                }
            });
        let destination = target.map(|(path, rect)| {
            ui.painter().rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0, ACCENT),
                egui::StrokeKind::Inside,
            );
            path.clone()
        });
        PaneActions {
            navigation,
            destination,
        }
    })
    .inner
}

fn explorer_drop_point(ui: &Ui) -> Option<Pos2> {
    // Winit's Windows file-drop events carry paths but omit the cursor position.
    let origin = ui.input(|input| input.viewport().inner_rect.map(|rect| rect.min))?;
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }
    let position = egui::vec2(cursor.x as f32, cursor.y as f32) / ui.ctx().pixels_per_point();
    Some(egui::pos2(position.x - origin.x, position.y - origin.y))
}

fn display_local(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    }
}

fn size_label(size: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match size {
        0..KIB => format!("{size} Б"),
        KIB..MIB => format!("{:.1} КиБ", size as f64 / KIB as f64),
        MIB..GIB => format!("{:.1} МиБ", size as f64 / MIB as f64),
        _ => format!("{:.1} ГиБ", size as f64 / GIB as f64),
    }
}
