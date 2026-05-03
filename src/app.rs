use crate::auth;
use crate::config::{LauncherConfig, app_dir, clean_player_name, minecraft_dir};
use crate::events::WorkerEvent;
use crate::launcher;
use eframe::egui::{
    self, Align, Color32, ColorImage, Context, FontData, FontDefinitions, FontFamily, Frame,
    Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, Vec2,
    ViewportCommand,
};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

pub struct GlideLauncherApp {
    config: LauncherConfig,
    events: Option<Receiver<WorkerEvent>>,
    logs: VecDeque<String>,
    progress_label: String,
    progress_current: u64,
    progress_total: u64,
    busy: bool,
    login_in_progress: bool,
    login_cancel_tx: Option<Sender<()>>,
    device_code: Option<DeviceCodeState>,
    icon: TextureHandle,
    java_edit: String,
    show_settings: bool,
    show_logs: bool,
    memory_limit_mb: u32,
    native_window_ready: bool,
}

#[derive(Debug, Clone)]
struct DeviceCodeState {
    verification_uri: String,
    user_code: String,
    message: String,
}

impl GlideLauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>, icon_bytes: &[u8]) -> Self {
        install_fonts(&cc.egui_ctx);

        let mut config = LauncherConfig::load();
        let memory_limit_mb = detect_memory_limit_mb();
        config.memory_mb = normalize_memory_mb(config.memory_mb, memory_limit_mb);

        let icon = load_icon_texture(&cc.egui_ctx, icon_bytes);
        let java_edit = config.java_path.clone();
        let mut logs = VecDeque::new();
        logs.push_back("Ready.".to_owned());

        Self {
            config,
            events: None,
            logs,
            progress_label: "Ready".to_owned(),
            progress_current: 0,
            progress_total: 1,
            busy: false,
            login_in_progress: false,
            login_cancel_tx: None,
            device_code: None,
            icon,
            java_edit,
            show_settings: false,
            show_logs: false,
            memory_limit_mb,
            native_window_ready: false,
        }
    }

    fn start_launch(&mut self) {
        if self.busy {
            return;
        }

        self.commit_edits();
        let (tx, rx) = mpsc::channel();
        let config = self.config.clone();
        self.events = Some(rx);
        self.busy = true;
        self.login_in_progress = false;
        self.login_cancel_tx = None;
        self.device_code = None;
        self.progress_label = "Preparing".to_owned();
        self.progress_current = 0;
        self.progress_total = 1;
        self.push_log("Launch requested.");

        thread::spawn(move || {
            let result = launcher::prepare_and_launch(config, tx.clone());
            match result {
                Ok(updated) => {
                    if let Some(account) = updated {
                        let _ = tx.send(WorkerEvent::AccountUpdated(account));
                    }
                    let _ = tx.send(WorkerEvent::Finished("Launch started.".to_owned()));
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::Failed(format!("{error:#}")));
                }
            }
        });
    }

    fn start_login(&mut self) {
        if self.busy {
            return;
        }

        self.commit_edits();
        let (tx, rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let client_id = self.config.microsoft_client_id.clone();
        self.events = Some(rx);
        self.busy = true;
        self.login_in_progress = true;
        self.login_cancel_tx = Some(cancel_tx);
        self.device_code = None;
        self.show_settings = true;
        self.progress_label = "Microsoft login".to_owned();
        self.progress_current = 0;
        self.progress_total = 1;
        self.push_log("Microsoft login requested.");

        thread::spawn(move || {
            let result = auth::device_login(&client_id, &tx, cancel_rx);
            match result {
                Ok(account) => {
                    let _ = tx.send(WorkerEvent::Authenticated(account));
                    let _ = tx.send(WorkerEvent::Finished(
                        "Microsoft login complete.".to_owned(),
                    ));
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if is_cancelled_message(&message) {
                        let _ = tx.send(WorkerEvent::Finished(
                            "Microsoft login cancelled.".to_owned(),
                        ));
                    } else {
                        let _ = tx.send(WorkerEvent::Failed(message));
                    }
                }
            }
        });
    }

    fn cancel_login(&mut self) {
        if !self.login_in_progress {
            return;
        }
        if let Some(cancel_tx) = self.login_cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
        self.progress_label = "Cancelling login...".to_owned();
        self.push_log("Cancelling Microsoft login...");
    }

    fn poll_events(&mut self) {
        let Some(rx) = self.events.take() else {
            return;
        };

        let mut keep_receiver = true;
        while let Ok(event) = rx.try_recv() {
            match event {
                WorkerEvent::Log(message) => self.push_log(message),
                WorkerEvent::Progress {
                    label,
                    current,
                    total,
                } => {
                    self.progress_label = label;
                    self.progress_current = current;
                    self.progress_total = total.max(1);
                }
                WorkerEvent::DeviceCode {
                    verification_uri,
                    user_code,
                    message,
                } => {
                    self.device_code = Some(DeviceCodeState {
                        verification_uri,
                        user_code,
                        message,
                    });
                    self.progress_label = "Enter code".to_owned();
                    self.show_settings = true;
                }
                WorkerEvent::Authenticated(account) | WorkerEvent::AccountUpdated(account) => {
                    self.config.account = Some(account);
                    if let Err(error) = self.config.save() {
                        self.push_log(format!("Failed to save settings: {error:#}"));
                    }
                }
                WorkerEvent::LaunchStarted(pid) => {
                    self.progress_label = format!("Running ({pid})");
                    self.push_log(format!("Minecraft started. PID {pid}."));
                }
                WorkerEvent::Finished(message) => {
                    self.finish_busy_state();
                    self.progress_current = self.progress_total;
                    self.push_log(message);
                    keep_receiver = false;
                }
                WorkerEvent::Failed(message) => {
                    self.finish_busy_state();
                    self.progress_label = "Failed".to_owned();
                    self.show_settings = true;
                    self.show_logs = true;
                    self.push_log(format!("ERROR: {message}"));
                    keep_receiver = false;
                }
            }
        }

        if keep_receiver {
            self.events = Some(rx);
        }
    }

    fn finish_busy_state(&mut self) {
        self.busy = false;
        self.login_in_progress = false;
        self.login_cancel_tx = None;
        self.device_code = None;
    }

    fn commit_edits(&mut self) {
        self.config.offline_name = clean_player_name(&self.config.offline_name);
        self.config.memory_mb = normalize_memory_mb(self.config.memory_mb, self.memory_limit_mb);
        self.config.java_path = self.java_edit.trim().to_owned();
        if let Err(error) = self.config.save() {
            self.push_log(format!("Failed to save settings: {error:#}"));
        }
    }

    fn push_log(&mut self, message: impl Into<String>) {
        while self.logs.len() > 120 {
            self.logs.pop_front();
        }
        self.logs.push_back(message.into());
    }

    fn progress_value(&self) -> f32 {
        (self.progress_current as f32 / self.progress_total.max(1) as f32).clamp(0.0, 1.0)
    }

    fn ensure_native_window_style(&mut self) {
        if self.native_window_ready {
            return;
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::Graphics::Dwm::{
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
            };
            use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

            let hwnd = unsafe { GetForegroundWindow() };
            if !hwnd.is_null() {
                let preference = DWMWCP_ROUND;
                let _ = unsafe {
                    DwmSetWindowAttribute(
                        hwnd,
                        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                        &preference as *const _ as *const core::ffi::c_void,
                        std::mem::size_of_val(&preference) as u32,
                    )
                };
                self.native_window_ready = true;
            }
        }

        #[cfg(not(windows))]
        {
            self.native_window_ready = true;
        }
    }
}

impl eframe::App for GlideLauncherApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        self.ensure_native_window_style();
        self.poll_events();

        egui::CentralPanel::default()
            .frame(Frame::new().fill(Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let rect = ui.max_rect().shrink(1.0);
                paint_background(ui, rect);
                self.title_bar(ctx, ui, rect);

                let content_rect = Rect::from_min_max(
                    Pos2::new(rect.left() + 24.0, rect.top() + 54.0),
                    Pos2::new(rect.right() - 24.0, rect.bottom() - 20.0),
                );
                self.sliding_content(ctx, ui, content_rect);
            });
    }
}

impl GlideLauncherApp {
    fn title_bar(&mut self, ctx: &Context, ui: &mut egui::Ui, rect: Rect) {
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 36.0));
        ui.painter().line_segment(
            [
                Pos2::new(bar.left() + 1.0, bar.bottom()),
                Pos2::new(bar.right() - 1.0, bar.bottom()),
            ],
            Stroke::new(1.0, BORDER),
        );

        let drag_rect = Rect::from_min_max(bar.min, Pos2::new(bar.right() - 80.0, bar.bottom()));
        let drag = ui.interact(
            drag_rect,
            ui.id().with("title-drag"),
            Sense::click_and_drag(),
        );
        if drag.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }

        ui.scope_builder(egui::UiBuilder::new().max_rect(bar.shrink(8.0)), |ui| {
            ui.horizontal(|ui| {
                ui.image((self.icon.id(), Vec2::splat(16.0)));
                ui.label(
                    RichText::new("Glide Launcher")
                        .size(13.0)
                        .strong()
                        .color(TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if traffic_button(ui, CLOSE).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    if traffic_button(ui, MINIMIZE).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                });
            });
        });
    }

    fn sliding_content(&mut self, ctx: &Context, ui: &mut egui::Ui, rect: Rect) {
        let t =
            ctx.animate_bool_with_time(ui.id().with("settings-slide"), self.show_settings, 0.18);
        let distance = rect.width() + 52.0;
        let home_rect = rect.translate(Vec2::new(-distance * t, 0.0));
        let settings_rect = rect.translate(Vec2::new(distance * (1.0 - t), 0.0));

        if !self.show_settings || t < 0.999 {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .id_salt("home-screen")
                    .max_rect(home_rect),
                |ui| {
                    ui.shrink_clip_rect(rect);
                    self.home(ui);
                },
            );
        }

        if self.show_settings || t > 0.001 {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .id_salt("settings-screen")
                    .max_rect(settings_rect),
                |ui| {
                    ui.shrink_clip_rect(rect);
                    self.settings_screen(ui);
                },
            );
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let controls_height = 66.0;
        let controls_rect = Rect::from_min_max(
            Pos2::new(rect.left(), rect.bottom() - controls_height),
            rect.right_bottom(),
        );
        let main_rect = Rect::from_min_max(
            rect.min,
            Pos2::new(rect.right(), controls_rect.top() - 12.0),
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(main_rect), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.image((self.icon.id(), Vec2::splat(116.0)));
                ui.add_space(15.0);
                ui.label(
                    RichText::new("Glide Client")
                        .size(31.0)
                        .strong()
                        .color(TEXT),
                );
                ui.label(RichText::new("Minecraft 1.8.9").size(13.0).color(MUTED));
                ui.add_space(18.0);
                ui.label(muted(self.progress_label.clone()));
                ui.add_space(5.0);
                draw_progress(ui, self.progress_value());
                ui.add_space(10.0);
                ui.label(muted(self.account_line()));
            });
        });

        ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if settings_icon_button(ui, Vec2::new(58.0, 58.0)).clicked() {
                    self.show_settings = true;
                }
                ui.add_space(8.0);

                let play_text = if self.busy {
                    "Working..."
                } else if self.config.account.is_some() {
                    "Launch"
                } else {
                    "Launch Offline"
                };

                let play = egui::Button::new(
                    RichText::new(play_text)
                        .size(23.0)
                        .strong()
                        .color(Color32::from_rgb(24, 24, 26)),
                )
                .fill(if self.busy { ACTION_DISABLED } else { ACTION })
                .stroke(Stroke::new(1.0, ACTION_LINE))
                .corner_radius(8.0)
                .min_size(Vec2::new(ui.available_width(), 58.0));

                if ui.add_enabled(!self.busy, play).clicked() {
                    self.start_launch();
                }
            });
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width());

        ui.horizontal(|ui| {
            if ui.add(secondary_button("Back")).clicked() {
                self.show_settings = false;
            }
            ui.add_space(8.0);
            ui.label(label("Settings"));
        });
        ui.add_space(14.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(9.0)
                    .inner_margin(Margin::same(13))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.account_settings(ui);
                        section_gap(ui);
                        self.runtime_settings(ui);
                        section_gap(ui);
                        self.folder_settings(ui);
                        section_gap(ui);
                        self.log_settings(ui);
                    });
            });
    }

    fn account_line(&self) -> String {
        match &self.config.account {
            Some(account) => format!("Signed in as {}", account.username()),
            None => format!(
                "Offline as {}",
                clean_player_name(&self.config.offline_name)
            ),
        }
    }

    fn account_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(label("Account"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.login_in_progress {
                    if ui.add(warn_button("Cancel")).clicked() {
                        self.cancel_login();
                    }
                } else if ui
                    .add_enabled(!self.busy, primary_button("Microsoft"))
                    .clicked()
                {
                    self.start_login();
                }
            });
        });

        match &self.config.account {
            Some(account) => {
                ui.add(egui::Label::new(code(account.username())).wrap());
                if ui
                    .add_enabled(!self.busy, secondary_button("Sign out"))
                    .clicked()
                {
                    self.config.account = None;
                    let _ = self.config.save();
                    self.push_log("Signed out.");
                }
            }
            None => {
                ui.label(muted("Offline name"));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.offline_name)
                            .desired_width(ui.available_width()),
                    )
                    .lost_focus()
                {
                    self.config.offline_name = clean_player_name(&self.config.offline_name);
                    let _ = self.config.save();
                }
            }
        }

        if let Some(code_state) = &self.device_code {
            ui.add_space(8.0);
            ui.label(muted("Enter this code at microsoft.com/link"));
            ui.label(
                RichText::new(&code_state.user_code)
                    .size(24.0)
                    .monospace()
                    .strong()
                    .color(TEXT),
            );
            ui.hyperlink_to("Open link", &code_state.verification_uri);
            ui.add(egui::Label::new(code(&code_state.message)).wrap());
            if self.login_in_progress && ui.add(warn_button("Cancel Login")).clicked() {
                self.cancel_login();
            }
        }
    }

    fn runtime_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(label("Memory"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(code(format!(
                    "{} / {} MB",
                    self.config.memory_mb, self.memory_limit_mb
                )));
            });
        });

        if ui
            .add(
                egui::Slider::new(&mut self.config.memory_mb, 512..=self.memory_limit_mb)
                    .step_by(256.0)
                    .show_value(false),
            )
            .changed()
        {
            self.config.memory_mb =
                normalize_memory_mb(self.config.memory_mb, self.memory_limit_mb);
            let _ = self.config.save();
        }

        if ui
            .checkbox(&mut self.config.use_bundled_java, "Bundled Java 8")
            .changed()
        {
            let _ = self.config.save();
        }

        ui.add_enabled_ui(!self.config.use_bundled_java, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.java_edit)
                            .desired_width(ui.available_width() - 78.0),
                    )
                    .lost_focus()
                {
                    self.commit_edits();
                }
                if ui.add(secondary_button("Browse")).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Java executable", &["exe"])
                        .pick_file()
                    {
                        self.java_edit = path.to_string_lossy().to_string();
                        self.commit_edits();
                    }
                }
            });
        });
    }

    fn folder_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(label("Folders"));
        let width = (ui.available_width() - 12.0) / 3.0;

        ui.horizontal(|ui| {
            if ui
                .add_sized([width, 30.0], secondary_button("Data"))
                .clicked()
            {
                let _ = open::that(app_dir());
            }
            if ui
                .add_sized([width, 30.0], secondary_button("Packs"))
                .clicked()
            {
                let _ = open::that(minecraft_dir().join("resourcepacks"));
            }
            if ui
                .add_sized([width, 30.0], secondary_button("Logs"))
                .clicked()
            {
                let _ = open::that(app_dir().join("logs"));
            }
        });
    }

    fn log_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(label("Status"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(secondary_button(if self.show_logs {
                        "Hide log"
                    } else {
                        "Show log"
                    }))
                    .clicked()
                {
                    self.show_logs = !self.show_logs;
                }
            });
        });

        let last = self
            .logs
            .back()
            .cloned()
            .unwrap_or_else(|| "Ready.".to_owned());
        ui.add(egui::Label::new(code(last)).wrap());

        if self.show_logs {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(110.0)
                .show(ui, |ui| {
                    for entry in &self.logs {
                        ui.add(egui::Label::new(code(entry)).wrap());
                    }
                });
        }
    }
}

const WINDOW_RADIUS: f32 = 14.0;
const BG: Color32 = Color32::from_rgb(6, 6, 7);
const PANEL: Color32 = Color32::from_rgb(15, 15, 17);
const TRACK: Color32 = Color32::from_rgb(227, 227, 229);
const ACCENT: Color32 = Color32::from_rgb(92, 183, 242);
const ACTION: Color32 = Color32::from_rgb(151, 185, 201);
const ACTION_DISABLED: Color32 = Color32::from_rgb(95, 104, 110);
const ACTION_LINE: Color32 = Color32::from_rgba_premultiplied(72, 72, 72, 72);
const TEXT: Color32 = Color32::from_rgb(246, 246, 248);
const MUTED: Color32 = Color32::from_rgb(171, 171, 175);
const BORDER: Color32 = Color32::from_rgba_premultiplied(39, 39, 42, 39);
const MINIMIZE: Color32 = Color32::from_rgb(242, 199, 76);
const CLOSE: Color32 = Color32::from_rgb(241, 97, 66);

fn paint_background(ui: &mut egui::Ui, rect: Rect) {
    ui.painter().rect(
        rect,
        WINDOW_RADIUS,
        BG,
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
}

fn section_gap(ui: &mut egui::Ui) {
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(10.0);
}

fn draw_progress(ui: &mut egui::Ui, progress: f32) {
    let width = ui.available_width().min(352.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    let painter = ui.painter();

    painter.rect(
        rect,
        999.0,
        TRACK,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 28)),
        StrokeKind::Inside,
    );

    let fill_width = (rect.width() * progress).clamp(0.0, rect.width());
    if fill_width > 0.0 {
        let fill_rect = Rect::from_min_max(
            rect.min,
            Pos2::new((rect.left() + fill_width).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(fill_rect, 999.0, ACCENT);
    }
}

fn settings_icon_button(ui: &mut egui::Ui, size: Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.hovered() {
        Color32::from_rgb(40, 40, 43)
    } else {
        Color32::from_rgb(28, 28, 31)
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        8.0,
        fill,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 52)),
        StrokeKind::Outside,
    );

    let left = rect.left() + 14.0;
    let right = rect.right() - 14.0;
    let center_y = rect.center().y;
    let ys = [center_y - 9.0, center_y, center_y + 9.0];
    let knob_x = [
        rect.center().x + 5.0,
        rect.center().x - 7.0,
        rect.center().x + 2.0,
    ];

    for i in 0..3 {
        painter.line_segment(
            [Pos2::new(left, ys[i]), Pos2::new(right, ys[i])],
            Stroke::new(1.8, TEXT),
        );
        painter.circle_filled(Pos2::new(knob_x[i], ys[i]), 3.8, TEXT);
    }

    response
}

fn traffic_button(ui: &mut egui::Ui, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let draw_color = if response.hovered() {
        lighten(color, 20)
    } else {
        color
    };
    ui.painter().circle_filled(rect.center(), 8.0, draw_color);
    response
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(20, 24, 29)),
    )
    .fill(Color32::from_rgb(128, 186, 222))
    .stroke(Stroke::new(
        1.0,
        Color32::from_rgba_unmultiplied(255, 255, 255, 38),
    ))
    .corner_radius(7.0)
    .min_size(Vec2::new(74.0, 30.0))
}

fn secondary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(12.0).color(TEXT))
        .fill(Color32::from_rgb(30, 30, 33))
        .stroke(Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 44),
        ))
        .corner_radius(7.0)
        .min_size(Vec2::new(74.0, 30.0))
}

fn warn_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(12.0).strong().color(TEXT))
        .fill(Color32::from_rgb(96, 42, 36))
        .stroke(Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(255, 170, 150, 80),
        ))
        .corner_radius(7.0)
        .min_size(Vec2::new(88.0, 30.0))
}

fn label(text: impl Into<String>) -> RichText {
    RichText::new(text).size(13.0).strong().color(TEXT)
}

fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(MUTED)
}

fn code(text: impl Into<String>) -> RichText {
    RichText::new(text).size(11.0).monospace().color(MUTED)
}

fn lighten(color: Color32, amount: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r().saturating_add(amount),
        color.g().saturating_add(amount),
        color.b().saturating_add(amount),
        color.a(),
    )
}

fn normalize_memory_mb(value: u32, max_value: u32) -> u32 {
    let max_value = max_value.max(512);
    let clamped = value.clamp(512, max_value);
    let stepped = (clamped / 256).max(2) * 256;
    stepped.min(max_value)
}

fn detect_memory_limit_mb() -> u32 {
    let total_mb = detect_total_memory_mb().unwrap_or(8192);
    let rounded = (total_mb / 256).max(2) * 256;
    rounded.clamp(1024, 65536)
}

#[cfg(windows)]
fn detect_total_memory_mb() -> Option<u32> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;

    let ok = unsafe { GlobalMemoryStatusEx(&mut status as *mut MEMORYSTATUSEX) };
    if ok == 0 {
        return None;
    }

    u32::try_from(status.ullTotalPhys / 1024 / 1024).ok()
}

#[cfg(not(windows))]
fn detect_total_memory_mb() -> Option<u32> {
    None
}

fn is_cancelled_message(message: &str) -> bool {
    message.to_ascii_lowercase().contains("cancel")
}

fn install_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "meiryo".to_owned(),
        FontData::from_static(include_bytes!("C:/Windows/Fonts/meiryo.ttc")).into(),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "meiryo".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "meiryo".to_owned());
    ctx.set_fonts(fonts);
}

fn load_icon_texture(ctx: &Context, bytes: &[u8]) -> TextureHandle {
    let image = image::load_from_memory(bytes)
        .expect("glideicon.png must be valid")
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let pixels = image.into_raw();
    ctx.load_texture(
        "glide-icon",
        ColorImage::from_rgba_unmultiplied(size, &pixels),
        egui::TextureOptions::LINEAR,
    )
}
