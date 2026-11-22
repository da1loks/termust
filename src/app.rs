use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align, Button, Color32, Frame, Id, Key, Layout, Margin, Modifiers, RichText, Ui,
};
use zeroize::Zeroizing;

use crate::connection::{Launch, LocalShell, Profile, Profiles, Protocol, SavedPassword};
use crate::credentials;
use crate::sftp::{Phase, SftpView};
use crate::terminal::{Renderer, Session, Status};
use crate::theme::{self, ACCENT, CANVAS, ERROR, MUTED, SELECTED, SIDEBAR, TEXT, TOOLBAR};

enum Action {
    Local(LocalShell),
    Remote(usize, Protocol),
    SelectTab(u64),
    Edit(Option<usize>),
}

struct Tab {
    id: u64,
    launch: Launch,
    content: TabContent,
}

enum TabContent {
    Terminal(Session),
    Files(Box<SftpView>),
}

pub struct App {
    active: Arc<AtomicU64>,
    tabs: Vec<Tab>,
    next_id: u64,
    focus_terminal: bool,
    renderer: Renderer,
    profiles: Option<Profiles>,
    editor: Option<Editor>,
    error: Option<String>,
}

impl App {
    pub fn new(context: &egui::Context) -> Self {
        theme::apply(context);
        let (profiles, error) = match Profiles::load() {
            Ok(profiles) => (Some(profiles), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let mut app = Self {
            active: Arc::new(AtomicU64::new(0)),
            tabs: Vec::new(),
            next_id: 1,
            focus_terminal: true,
            renderer: Renderer::default(),
            profiles,
            editor: None,
            error,
        };
        app.launch(context, LocalShell::PowerShell.launch());
        app
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active.load(Ordering::Relaxed);
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn select(&mut self, id: u64) {
        self.active.store(id, Ordering::Relaxed);
        self.focus_terminal = true;
    }

    fn open(&mut self, context: &egui::Context, launch: Launch) -> Result<()> {
        let id = self.next_id;
        let content = if launch.transport == "SFTP" {
            TabContent::Files(Box::new(SftpView::open(
                launch.clone(),
                id,
                self.active.clone(),
                context.clone(),
            )?))
        } else {
            TabContent::Terminal(Session::open(
                id,
                &launch,
                self.active.clone(),
                context.clone(),
            )?)
        };
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            launch,
            content,
        });
        self.select(id);
        Ok(())
    }

    fn launch(&mut self, context: &egui::Context, launch: Result<Launch>) {
        if let Err(error) = launch.and_then(|launch| self.open(context, launch)) {
            self.error = Some(format!("{error:#}"));
        }
    }

    fn launch_profile(&mut self, context: &egui::Context, index: usize, protocol: Protocol) {
        if let Some(profile) = self
            .profiles
            .as_ref()
            .and_then(|store| store.items.get(index))
        {
            self.launch(context, protocol.launch(profile));
        }
    }

    fn close(&mut self, index: usize) {
        let was_active = self.active_index() == Some(index);
        self.tabs.remove(index);
        if was_active {
            self.select(
                self.tabs
                    .get(index.min(self.tabs.len().saturating_sub(1)))
                    .map_or(0, |tab| tab.id),
            );
        }
    }

    fn shortcuts(&mut self, context: &egui::Context) {
        if self.editor.is_some() {
            return;
        }
        let (new_tab, close_tab, previous, next) = context.input_mut(|input| {
            (
                input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::T),
                input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::W),
                input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab),
                input.consume_key(Modifiers::CTRL, Key::Tab),
            )
        });
        if new_tab {
            self.launch(context, LocalShell::PowerShell.launch());
        }
        if let Some(index) = self.active_index() {
            if close_tab {
                self.close(index);
            } else if previous || next {
                let len = self.tabs.len();
                let offset = if previous { len - 1 } else { 1 };
                self.select(self.tabs[(index + offset) % len].id);
            }
        }
    }

    fn sidebar(&self, ui: &mut Ui) -> Option<Action> {
        let mut action = None;
        ui.label(RichText::new("Сеансы").size(15.0).strong());
        ui.add_space(14.0);
        ui.label(RichText::new("Локальные оболочки").color(MUTED).size(11.5));
        ui.add_space(3.0);
        for shell in LocalShell::ALL {
            let row = flat_button(shell.label()).min_size(egui::vec2(ui.available_width(), 26.0));
            if ui.add(row).clicked() {
                action = Some(Action::Local(shell));
            }
        }
        ui.add_space(9.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Серверы").size(11.5).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(flat_button("+ SSH")).clicked() {
                    action = Some(Action::Edit(None));
                }
            });
        });
        let mut sessions = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab.launch.transport, "SSH" | "SFTP"))
            .peekable();
        let profiles = self
            .profiles
            .as_ref()
            .map_or(&[][..], |store| store.items.as_slice());
        let show_sections = sessions.peek().is_some() && !profiles.is_empty();
        let active = self.active.load(Ordering::Relaxed);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if show_sections {
                    ui.label(RichText::new("Сохранённые").size(11.0).color(MUTED));
                }
                for (index, profile) in profiles.iter().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let name_width = (ui.available_width() - 60.0).max(1.0);
                            let name = ui
                                .allocate_ui_with_layout(
                                    egui::vec2(name_width, 26.0),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        ui.set_width(name_width);
                                        ui.add(
                                            flat_button(profile.label())
                                                .truncate()
                                                .min_size(egui::vec2(name_width, 26.0)),
                                        )
                                    },
                                )
                                .inner;
                            if name
                                .on_hover_text(format!(
                                    "{}\nОткрыть SSH · {}",
                                    profile.label(),
                                    profile.host
                                ))
                                .clicked()
                            {
                                action = Some(Action::Remote(index, Protocol::Ssh));
                            }
                            ui.spacing_mut().button_padding.x = 4.0;
                            let sftp = flat_button(RichText::new("SFTP").size(10.5).color(MUTED))
                                .min_size(egui::vec2(32.0, 26.0));
                            let edit = flat_button(RichText::new("···").color(MUTED))
                                .min_size(egui::vec2(24.0, 26.0));
                            if ui.add(sftp).clicked() {
                                action = Some(Action::Remote(index, Protocol::Sftp));
                            }
                            if ui.add(edit).on_hover_text("Изменить профиль").clicked()
                            {
                                action = Some(Action::Edit(Some(index)));
                            }
                        });
                    });
                }
                if show_sections {
                    ui.add_space(5.0);
                    ui.label(RichText::new("Открытые").size(11.0).color(MUTED));
                }
                for tab in sessions {
                    ui.push_id(("open-session", tab.id), |ui| {
                        let tint = if tab.id == active { TEXT } else { MUTED };
                        let title = RichText::new(&tab.launch.title).color(tint);
                        let protocol = RichText::new(tab.launch.transport).size(10.5).color(MUTED);
                        let row = flat_button(title)
                            .right_text(protocol)
                            .min_size(egui::vec2(ui.available_width(), 26.0))
                            .truncate();
                        if ui.add(row).clicked() {
                            action = Some(Action::SelectTab(tab.id));
                        }
                    });
                }
            });
        action
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        const TAB_HEIGHT: f32 = 29.0;
        const LABEL_PADDING: f32 = 6.0;
        const CLOSE_WIDTH: f32 = 21.0;
        const TAB_GAP: f32 = 6.0;
        const ADD_WIDTH: f32 = 24.0;
        let current = self.active.load(Ordering::Relaxed);
        let mut select = None;
        let mut close = None;
        let mut new_tab = false;
        egui::ScrollArea::horizontal()
            .id_salt("tabs")
            .show(ui, |ui| {
                let font = egui::TextStyle::Button.resolve(ui.style());
                let label_widths = ui.fonts_mut(|fonts| {
                    self.tabs
                        .iter()
                        .map(|tab| {
                            fonts
                                .layout_no_wrap(tab.launch.title.clone(), font.clone(), TEXT)
                                .size()
                                .x
                                .ceil()
                                .clamp(28.0, 152.0)
                        })
                        .collect::<Vec<_>>()
                });
                let width = label_widths.iter().sum::<f32>()
                    + label_widths.len() as f32 * (LABEL_PADDING * 2.0 + CLOSE_WIDTH + TAB_GAP)
                    + ADD_WIDTH;
                let (strip, _) =
                    ui.allocate_exact_size(egui::vec2(width, TAB_HEIGHT), egui::Sense::hover());
                let mut left = strip.left();
                for (index, (tab, label_width)) in self.tabs.iter().zip(label_widths).enumerate() {
                    let title = egui::Rect::from_min_size(
                        egui::pos2(left + LABEL_PADDING, strip.top()),
                        egui::vec2(label_width, TAB_HEIGHT),
                    );
                    let close_button = egui::Rect::from_min_size(
                        egui::pos2(title.right() + LABEL_PADDING, strip.top()),
                        egui::vec2(CLOSE_WIDTH, TAB_HEIGHT),
                    );
                    let mut tab_ui = ui.new_child(egui::UiBuilder::new().id_salt(tab.id));
                    if tab_ui
                        .place(title, flat_button(tab.launch.title.as_str()).truncate())
                        .clicked()
                    {
                        select = Some(tab.id);
                    }
                    if tab_ui.place(close_button, flat_button("×")).clicked() {
                        close = Some(index);
                    }
                    if tab.id == current {
                        ui.painter().hline(
                            left..=close_button.right(),
                            strip.bottom() - 1.0,
                            egui::Stroke::new(1.0, MUTED),
                        );
                    }
                    left = close_button.right() + TAB_GAP;
                }
                let add_button = egui::Rect::from_min_size(
                    egui::pos2(left, strip.top()),
                    egui::vec2(ADD_WIDTH, TAB_HEIGHT),
                );
                new_tab = ui
                    .place(add_button, flat_button("+"))
                    .on_hover_text("Новый PowerShell · Ctrl+Shift+T")
                    .clicked();
            });
        if let Some(id) = select {
            self.select(id);
        }
        if let Some(index) = close {
            self.close(index);
        }
        if new_tab {
            self.launch(ui.ctx(), LocalShell::PowerShell.launch());
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let Some(index) = self.active_index() else {
            return;
        };
        let tab = &self.tabs[index];
        let (tint, detail, size, can_retry) = match &tab.content {
            TabContent::Terminal(session) => {
                let size = Some(session.dimensions());
                match session.status() {
                    Status::Running => (ACCENT, None, size, false),
                    Status::Exited(status) => {
                        let label = match status.and_then(|status| status.code()) {
                            Some(code) => format!("Сеанс завершён · код {code}"),
                            None => "Сеанс завершён".into(),
                        };
                        (MUTED, Some(label), size, true)
                    }
                }
            }
            TabContent::Files(view) => match view.phase() {
                Phase::Connecting => (MUTED, Some("Подключение…".into()), None, false),
                Phase::Connected => (ACCENT, None, None, false),
                Phase::Closed => (MUTED, Some("Сеанс завершён".into()), None, true),
            },
        };
        let mut retry = false;
        ui.horizontal(|ui| {
            let (mark, _) = ui.allocate_exact_size(egui::vec2(7.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(mark.center(), 2.5, tint);
            ui.label(RichText::new(tab.launch.transport).size(11.5).color(MUTED));
            if let Some(detail) = detail {
                ui.separator();
                ui.label(RichText::new(detail).size(11.5).color(MUTED));
            }
            if can_retry {
                retry = ui.add(flat_button("Открыть снова")).clicked();
            }
            if let Some((cols, rows)) = size {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{cols} × {rows}"))
                            .monospace()
                            .size(11.5)
                            .color(MUTED),
                    );
                });
            }
        });
        if retry {
            self.launch(ui.ctx(), Ok(self.tabs[index].launch.clone()));
        }
    }

    fn edit_profile(&mut self, context: &egui::Context) {
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        let response = editor.show(context, self.profiles.is_some());
        let Some(action) = response.inner else {
            if response.should_close() {
                self.focus_terminal = true;
            } else {
                self.editor = Some(editor);
            }
            return;
        };
        let result = self.apply_profile_action(context, &editor, action);
        if let Err(error) = result {
            editor.error = Some(format!("{error:#}"));
            self.editor = Some(editor);
        } else {
            self.focus_terminal = true;
        }
    }

    fn apply_profile_action(
        &mut self,
        context: &egui::Context,
        editor: &Editor,
        action: EditorAction,
    ) -> Result<()> {
        match action {
            EditorAction::Connect(protocol) => {
                let mut profile = editor.profile()?;
                let existing = editor
                    .index
                    .and_then(|index| self.profiles.as_ref()?.items.get(index));
                editor.restore_password(&mut profile, existing);
                let launch = protocol.launch_with_password(&profile, &editor.password)?;
                self.open(context, launch)
            }
            EditorAction::Save | EditorAction::SaveAndConnect => {
                let index = self.save_profile(editor)?;
                if matches!(action, EditorAction::SaveAndConnect) {
                    self.launch_profile(context, index, Protocol::Ssh);
                }
                Ok(())
            }
            EditorAction::Delete => self.write_profile(editor.index, None, ""),
            EditorAction::Cancel => Ok(()),
        }
    }

    fn save_profile(&mut self, editor: &Editor) -> Result<usize> {
        let mut profile = editor.profile()?;
        let store = self
            .profiles
            .as_ref()
            .context("Хранилище профилей недоступно")?;
        let index = editor.index.or_else(|| {
            store.items.iter().position(|saved| {
                saved.same_login(&profile)
                    && saved.identity_file == profile.identity_file
                    && (profile.name.is_empty() || profile.name == saved.name)
            })
        });
        let existing = index.and_then(|index| store.items.get(index));
        if editor.index.is_none()
            && profile.name.is_empty()
            && let Some(saved) = existing
        {
            profile.name = saved.name.clone();
        }
        editor.restore_password(&mut profile, existing);
        let saved_index = index.unwrap_or(store.items.len());
        self.write_profile(index, Some(profile), &editor.password)?;
        Ok(saved_index)
    }

    fn write_profile(
        &mut self,
        index: Option<usize>,
        mut profile: Option<Profile>,
        password: &str,
    ) -> Result<()> {
        let store = self
            .profiles
            .as_mut()
            .context("Хранилище профилей недоступно")?;
        let old_id = index
            .and_then(|index| store.items[index].password.as_ref())
            .map(|password| password.id);
        let mut created = None;
        if let Some(profile) = &mut profile
            && !password.is_empty()
        {
            let endpoint = profile.effective_endpoint()?;
            let id = credentials::save(password)?;
            profile.password = Some(SavedPassword { id, endpoint });
            created = Some(id);
        }
        let new_id = profile
            .as_ref()
            .and_then(|profile| profile.password.as_ref())
            .map(|password| password.id);
        let mut items = store.items.clone();
        match (index, profile) {
            (Some(index), Some(profile)) => items[index] = profile,
            (None, Some(profile)) => items.push(profile),
            (Some(index), None) => {
                items.remove(index);
            }
            (None, None) => return Ok(()),
        }
        if let Err(error) = store.replace(items) {
            if let Some(id) = created {
                let _ = credentials::remove(id);
            }
            return Err(error);
        }
        if let Some(id) = old_id
            && Some(id) != new_id
            && let Err(error) = credentials::remove(id)
        {
            self.error = Some(format!("{error:#}"));
        }
        Ok(())
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.shortcuts(&context);
        let sidebar = egui::Panel::left("profiles")
            .resizable(false)
            .exact_size(222.0)
            .frame(panel(SIDEBAR, 12, 14))
            .show(ui, |ui| self.sidebar(ui));
        if let Some(action) = sidebar.inner {
            match action {
                Action::Local(shell) => self.launch(&context, shell.launch()),
                Action::Remote(index, protocol) => self.launch_profile(&context, index, protocol),
                Action::SelectTab(id) => self.select(id),
                Action::Edit(index) => {
                    let profile = index
                        .and_then(|index| self.profiles.as_ref()?.items.get(index).cloned())
                        .unwrap_or_default();
                    self.editor = Some(Editor::new(index, profile));
                }
            }
        }

        egui::Panel::top("tab-bar")
            .resizable(false)
            .frame(panel(TOOLBAR, 9, 4))
            .show(ui, |ui| self.tab_bar(ui));
        if let Some(error) = &self.error {
            let dismiss = egui::Panel::top("error")
                .resizable(false)
                .frame(panel(SIDEBAR, 10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let dismiss = ui
                            .add(flat_button("×"))
                            .on_hover_text("Закрыть сообщение")
                            .clicked();
                        ui.label(RichText::new(error).color(ERROR));
                        dismiss
                    })
                    .inner
                })
                .inner;
            if dismiss {
                self.error = None;
            }
        }
        if let Some(index) = self.active_index()
            && let TabContent::Files(view) = &mut self.tabs[index].content
        {
            view.update();
        }
        egui::Panel::bottom("status-bar")
            .resizable(false)
            .frame(panel(TOOLBAR, 11, 4))
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(panel(CANVAS, 10, 8))
            .show(ui, |ui| {
                if let Some(index) = self.active_index() {
                    let tab = &mut self.tabs[index];
                    let enabled = self.editor.is_none();
                    let focus = enabled && std::mem::take(&mut self.focus_terminal);
                    ui.push_id(tab.id, |ui| match &mut tab.content {
                        TabContent::Terminal(session) => {
                            session.show(ui, &mut self.renderer, focus, enabled);
                        }
                        TabContent::Files(view) => {
                            ui.add_enabled_ui(enabled, |ui| view.show(ui));
                        }
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Открой локальную оболочку или подключись к серверу")
                                .color(MUTED),
                        );
                    });
                }
            });
        self.edit_profile(&context);
    }
}

fn panel(fill: Color32, x: i8, y: i8) -> Frame {
    Frame::new()
        .fill(fill)
        .inner_margin(Margin::symmetric(x, y))
}

fn flat_button(label: impl Into<egui::WidgetText>) -> Button<'static> {
    Button::new(label.into()).frame(false)
}

enum EditorAction {
    Connect(Protocol),
    Save,
    SaveAndConnect,
    Delete,
    Cancel,
}

struct Editor {
    index: Option<usize>,
    draft: Profile,
    port: String,
    password: Zeroizing<String>,
    remember_ssh: bool,
    forget_password: bool,
    focus_host: bool,
    error: Option<String>,
}

impl Editor {
    fn new(index: Option<usize>, draft: Profile) -> Self {
        Self {
            index,
            port: draft.port.map_or_else(String::new, |port| port.to_string()),
            password: Zeroizing::new(String::new()),
            remember_ssh: true,
            forget_password: false,
            draft,
            focus_host: true,
            error: None,
        }
    }

    fn profile(&self) -> Result<Profile> {
        let port = self.port.trim();
        let profile = Profile {
            name: self.draft.name.trim().into(),
            host: self.draft.host.trim().into(),
            user: self.draft.user.trim().into(),
            port: if port.is_empty() {
                None
            } else {
                Some(port.parse().context("Порт должен быть от 1 до 65535")?)
            },
            identity_file: self.draft.identity_file.trim().into(),
            password: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    fn restore_password(&self, profile: &mut Profile, existing: Option<&Profile>) {
        if let Some(existing) = existing
            && !self.forget_password
            && existing.same_login(profile)
        {
            profile.password = existing.password.clone();
        }
    }

    fn show(
        &mut self,
        context: &egui::Context,
        can_save: bool,
    ) -> egui::ModalResponse<Option<EditorAction>> {
        let frame =
            Frame::popup(&context.style_of(egui::Theme::Dark)).inner_margin(Margin::same(16));
        egui::Modal::new(Id::new("profile-editor"))
            .frame(frame)
            .backdrop_color(Color32::from_black_alpha(145))
            .show(context, |ui| {
                ui.set_width(454.0);
                let title = if self.index.is_some() {
                    "Профиль подключения"
                } else {
                    "Новое SSH-подключение"
                };
                ui.heading(title);
                ui.add_space(12.0);
                egui::Grid::new("profile-fields")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Адрес сервера");
                        let host = ui.add(profile_field(
                            &mut self.draft.host,
                            "IP, домен или SSH-алиас",
                        ));
                        if std::mem::take(&mut self.focus_host) {
                            host.request_focus();
                        }
                        ui.end_row();
                        ui.label("Пользователь");
                        ui.add(profile_field(&mut self.draft.user, "Из настроек SSH"));
                        ui.end_row();
                        ui.label("Порт");
                        ui.add(profile_field(&mut self.port, "Из настроек SSH"));
                        ui.end_row();
                        ui.label("Пароль SSH");
                        let hint = if self.draft.password.is_some() && !self.forget_password {
                            "Сохранён. Введи новый для замены"
                        } else {
                            "Необязательно"
                        };
                        let password = ui.add(
                            profile_field(&mut self.password, hint)
                                .password(true)
                                .char_limit(1023),
                        );
                        if password.changed() && !self.password.is_empty() {
                            self.forget_password = false;
                        }
                        ui.end_row();
                        ui.label("Файл ключа");
                        ui.add(profile_field(
                            &mut self.draft.identity_file,
                            "Необязательно",
                        ));
                        ui.end_row();
                        ui.label("Название");
                        ui.add(profile_field(&mut self.draft.name, "Как у хоста"));
                        ui.end_row();
                    });
                if self.draft.password.is_some() && self.password.is_empty() {
                    ui.checkbox(&mut self.forget_password, "Забыть сохранённый пароль");
                }
                ui.add_enabled(
                    can_save,
                    egui::Checkbox::new(
                        &mut self.remember_ssh,
                        "Запомнить SSH-подключение в «Серверах»",
                    ),
                );
                ui.label(
                    RichText::new(
                        "Сохранённый пароль хранится в диспетчере учётных данных Windows.",
                    )
                    .size(11.0)
                    .color(MUTED),
                );
                if let Some(error) = &self.error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(ERROR));
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(Button::new("Открыть SSH").fill(SELECTED)).clicked() {
                        return Some(if can_save && self.remember_ssh {
                            EditorAction::SaveAndConnect
                        } else {
                            EditorAction::Connect(Protocol::Ssh)
                        });
                    }
                    if ui.add(flat_button("Открыть SFTP")).clicked() {
                        return Some(EditorAction::Connect(Protocol::Sftp));
                    }
                    if ui.add_enabled(can_save, Button::new("Сохранить")).clicked() {
                        return Some(EditorAction::Save);
                    }
                    if self.index.is_some()
                        && ui
                            .add(flat_button(RichText::new("Удалить").color(ERROR)))
                            .clicked()
                    {
                        return Some(EditorAction::Delete);
                    }
                    if ui.add(flat_button("Отмена")).clicked() {
                        return Some(EditorAction::Cancel);
                    }
                    None
                })
                .inner
            })
    }
}

fn profile_field<'a>(value: &'a mut String, hint: &str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(value)
        .hint_text(RichText::new(hint).color(MUTED))
        .desired_width(344.0)
        .min_size(egui::vec2(0.0, 29.0))
        .margin(Margin::symmetric(8, 5))
}
