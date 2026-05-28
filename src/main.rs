#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

// hide console window on Windows in release
use eframe::egui;
use eframe::{App, NativeOptions, run_native};
use image::{GenericImageView, ImageFormat};
use reqwest::blocking::Client;
use scraper::{Html, Selector};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use version_compare::Version;
use winreg::RegKey;
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_32KEY,
    KEY_WOW64_64KEY,
};
use winrt_notification::{Duration as ToastDuration, Toast};

const DEFAULT_ADOBE_FOLDER: &str = r"C:\Program Files\Adobe";
const APP_ICON_RESOURCE_ID: u16 = 1;
const RUN_VALUE_NAME: &str = "M0nkrusAdobeTracker";
const SETTINGS_KEY: &str = r"Software\M0nkrus Adobe Tracker";
const BACKGROUND_ENABLED_VALUE: &str = "BackgroundUpdatesEnabled";
const LAST_BACKGROUND_CHECK_VALUE: &str = "LastBackgroundCheck";
const DAILY_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const TRACKER_URL: &str = "http://rutracker.ru/tracker.php?pid=1334502";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";
const TRAY_REFRESH_ID: &str = "refresh";
const TRAY_START_BOOT_ID: &str = "start_on_boot";
const TRAY_BACKGROUND_ID: &str = "background_updates";
const TRAY_EXIT_ID: &str = "exit";

fn main() -> Result<(), eframe::Error> {
    create_ui()
}

fn create_ui() -> Result<(), eframe::Error> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size(egui::vec2(980.0, 680.0))
        .with_min_inner_size(egui::vec2(720.0, 460.0));
    let viewport = if let Some(icon) = egui_icon_from_ico() {
        viewport.with_icon(icon)
    } else {
        viewport
    };

    //egui
    let options = NativeOptions {
        viewport,
        ..Default::default()
    };
    let default_path = PathBuf::from(DEFAULT_ADOBE_FOLDER);
    let path = default_path.exists().then_some(default_path);
    let start_on_boot = start_on_boot_enabled();
    let background_updates = background_updates_enabled();
    let (background_tx, background_rx) = mpsc::channel();
    let background_checker =
        BackgroundChecker::start(background_tx, path.clone(), background_updates);
    let app = MonkrusApp {
        local_app_list: None,
        online_app_list: None,
        path,
        status: None,
        online_error: None,
        tray: None,
        start_on_boot,
        background_updates,
        background_checker,
        background_rx,
        background_status: None,
        minimized_to_tray: false,
        exiting: false,
    };
    run_native(
        "M0nkrus Adobe Tracker",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
}

fn egui_icon_from_ico() -> Option<egui::IconData> {
    let image =
        image::load_from_memory_with_format(include_bytes!("../assets/icon.ico"), ImageFormat::Ico)
            .ok()?;
    let (width, height) = image.dimensions();
    let rgba = image.into_rgba8().into_raw();
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

struct MonkrusApp {
    local_app_list: Option<Vec<LocalFoundApp>>,
    online_app_list: Option<Vec<OnlineFoundApp>>,
    path: Option<PathBuf>,
    status: Option<String>,
    online_error: Option<String>,
    tray: Option<TrayState>,
    start_on_boot: bool,
    background_updates: bool,
    background_checker: BackgroundChecker,
    background_rx: Receiver<BackgroundMessage>,
    background_status: Option<String>,
    minimized_to_tray: bool,
    exiting: bool,
}
impl App for MonkrusApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        style_ui(ui.ctx());
        ui.ctx().request_repaint_after(Duration::from_millis(250));

        egui::Frame::central_panel(ui.style())
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| self.content_ui(ui, frame));
    }
}
impl MonkrusApp {
    fn content_ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.ensure_tray();
        self.handle_tray_events(ui.ctx());
        self.handle_tray_icon_events(ui.ctx());
        self.handle_background_messages();
        self.handle_minimize_to_tray(ui.ctx(), frame);

        if self.path.is_none() {
            self.file_dialog_update(ui, frame);
            return;
        }

        let path = self.path.as_ref().unwrap().clone();
        if self.local_app_list.is_none() {
            let local_apps = find_local_programs(&path);
            if local_apps.is_empty() {
                self.status = Some(format!(
                    "No installed Adobe products were found under {}.",
                    path.display()
                ));
                self.path = None;
                self.background_checker.set_path(None);
            } else {
                self.local_app_list = Some(local_apps);
            }
            return;
        }

        if self.online_app_list.is_none() && self.online_error.is_none() {
            match find_online_programs() {
                Ok(mut online_apps) => {
                    let local_app_list = self.local_app_list.as_mut().unwrap();
                    compare_versions(local_app_list, &mut online_apps);
                    self.online_app_list = Some(online_apps);
                    if self.background_status.as_deref() == Some("Refreshing...") {
                        self.background_status = Some("Refresh completed.".to_owned());
                    }
                }
                Err(error) => self.online_error = Some(error),
            }
        }

        let path_display = path.display().to_string();
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.heading("M0nkrus Adobe Tracker");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
            });
        });
        ui.label(format!("Adobe installation path: {path_display}"));
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.start_on_boot, "Start on boot")
                .changed()
            {
                if let Err(error) = set_start_on_boot(self.start_on_boot) {
                    self.background_status =
                        Some(format!("Could not update startup setting: {error}"));
                    self.start_on_boot = start_on_boot_enabled();
                }
                if let Some(tray) = &self.tray {
                    tray.start_on_boot.set_checked(self.start_on_boot);
                }
            }
            if ui
                .checkbox(
                    &mut self.background_updates,
                    "Check for updates in background daily (System tray & Windows notification)",
                )
                .changed()
            {
                set_background_updates_enabled(self.background_updates);
                self.background_checker.set_enabled(self.background_updates);
                if let Some(tray) = &self.tray {
                    tray.background_updates.set_checked(self.background_updates);
                }
            }
        });
        if let Some(status) = &self.background_status {
            ui.label(status);
        }
        ui.add_space(8.0);

        if let Some(error) = self.online_error.clone() {
            match online_error_dialog(ui, &error) {
                OnlineErrorAction::Retry => {
                    self.refresh_online();
                }
                OnlineErrorAction::ChangeFolder => {
                    self.path = None;
                    self.local_app_list = None;
                    self.online_app_list = None;
                    self.online_error = None;
                    self.background_checker.set_path(None);
                }
                OnlineErrorAction::None => {}
            }
            return;
        }

        let Some(local_app_list) = self.local_app_list.as_ref() else {
            return;
        };
        let Some(online_app_list) = self.online_app_list.as_ref() else {
            return;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_header(ui, "Updates");
            updates_header(ui);
            for (index, local_app) in local_app_list.iter().enumerate() {
                update_row(ui, local_app, index);
            }

            ui.add_space(22.0);
            section_header(ui, "Online found apps");
            online_apps_header(ui);
            for (index, online_app) in online_app_list.iter().enumerate() {
                online_app_row(ui, online_app, index);
            }
        });
    }

    fn ensure_tray(&mut self) {
        if self.tray.is_none() {
            match TrayState::new(self.start_on_boot, self.background_updates) {
                Ok(tray) => self.tray = Some(tray),
                Err(error) => {
                    self.background_status = Some(format!("Could not create system tray: {error}"));
                }
            }
        }
    }

    fn handle_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id().as_ref();
            match id {
                TRAY_REFRESH_ID => self.refresh(),
                TRAY_START_BOOT_ID => {
                    self.start_on_boot = !self.start_on_boot;
                    if let Err(error) = set_start_on_boot(self.start_on_boot) {
                        self.background_status =
                            Some(format!("Could not update startup setting: {error}"));
                        self.start_on_boot = start_on_boot_enabled();
                    }
                    if let Some(tray) = &self.tray {
                        tray.start_on_boot.set_checked(self.start_on_boot);
                    }
                }
                TRAY_BACKGROUND_ID => {
                    self.background_updates = !self.background_updates;
                    set_background_updates_enabled(self.background_updates);
                    self.background_checker.set_enabled(self.background_updates);
                    if let Some(tray) = &self.tray {
                        tray.background_updates.set_checked(self.background_updates);
                    }
                }
                TRAY_EXIT_ID => {
                    self.exiting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    fn handle_tray_icon_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                self.show_from_tray(ctx);
            }
        }
    }

    fn show_from_tray(&mut self, ctx: &egui::Context) {
        self.minimized_to_tray = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn handle_minimize_to_tray(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.background_updates {
            return;
        }

        if !self.exiting && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray(ctx, frame);
            return;
        }

        let minimized = ctx.input(|input| input.viewport().minimized == Some(true));
        if minimized && !self.minimized_to_tray {
            self.hide_to_tray(ctx, frame);
        }
    }

    fn hide_to_tray(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.minimized_to_tray = true;
        if let Some(window) = frame.winit_window() {
            window.set_minimized(false);
            window.set_visible(false);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn handle_background_messages(&mut self) {
        while let Ok(message) = self.background_rx.try_recv() {
            match message {
                BackgroundMessage::Checked { updates_found } => {
                    self.background_status = Some(if updates_found == 0 {
                        "Background check completed: no updates found.".to_owned()
                    } else {
                        format!("Background check completed: {updates_found} update(s) found.")
                    });
                }
                BackgroundMessage::Error(error) => {
                    self.background_status = Some(format!("Background check failed: {error}"));
                }
            }
        }
    }

    fn refresh(&mut self) {
        self.local_app_list = None;
        self.online_app_list = None;
        self.online_error = None;
        self.background_status = Some("Refreshing...".to_owned());
    }

    fn refresh_online(&mut self) {
        if let Some(local_app_list) = self.local_app_list.as_mut() {
            for local_app in local_app_list {
                local_app.newest_online = None;
            }
        }
        self.online_error = None;
        self.online_app_list = None;
    }

    fn file_dialog_update(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.2).min(120.0));
            ui.heading("Adobe folder location");
            if let Some(status) = &self.status {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(210, 95, 55), status);
            } else if !Path::new(DEFAULT_ADOBE_FOLDER).exists() {
                ui.add_space(8.0);
                ui.colored_label(
                    egui::Color32::from_rgb(210, 95, 55),
                    format!("{DEFAULT_ADOBE_FOLDER} was not found."),
                );
            }
            ui.label("Select the folder that contains your installed Adobe applications.");
            ui.add_space(12.0);
            if ui.button("Select folder").clicked() {
                let path = rfd::FileDialog::new().pick_folder();
                self.path = path;
                self.local_app_list = None;
                self.online_app_list = None;
                self.status = None;
                self.online_error = None;
                self.background_checker.set_path(self.path.clone());
            }
        });
    }
}

#[derive(PartialEq, Eq)]
enum OnlineErrorAction {
    None,
    Retry,
    ChangeFolder,
}

struct TrayState {
    _tray_icon: TrayIcon,
    start_on_boot: CheckMenuItem,
    background_updates: CheckMenuItem,
}

impl TrayState {
    fn new(start_on_boot: bool, background_updates: bool) -> Result<Self, String> {
        let menu = Menu::new();
        let refresh = MenuItem::with_id(TRAY_REFRESH_ID, "Refresh", true, None);
        let start_on_boot_item = CheckMenuItem::with_id(
            TRAY_START_BOOT_ID,
            "Start on boot",
            true,
            start_on_boot,
            None,
        );
        let background_updates_item = CheckMenuItem::with_id(
            TRAY_BACKGROUND_ID,
            "Run background checks",
            true,
            background_updates,
            None,
        );
        let exit = MenuItem::with_id(TRAY_EXIT_ID, "Exit", true, None);

        menu.append_items(&[
            &refresh,
            &PredefinedMenuItem::separator(),
            &start_on_boot_item,
            &background_updates_item,
            &PredefinedMenuItem::separator(),
            &exit,
        ])
        .map_err(|error| format!("Could not build tray menu: {error}"))?;

        let icon = tray_icon_from_resource().or_else(|_| tray_icon_rgba())?;
        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip("M0nkrus Adobe Tracker")
            .with_icon(icon)
            .build()
            .map_err(|error| format!("Could not build tray icon: {error}"))?;

        Ok(Self {
            _tray_icon: tray_icon,
            start_on_boot: start_on_boot_item,
            background_updates: background_updates_item,
        })
    }
}

fn tray_icon_from_resource() -> Result<Icon, String> {
    Icon::from_resource(APP_ICON_RESOURCE_ID, Some((16, 16)))
        .map_err(|error| format!("Could not load embedded tray icon resource: {error}"))
}

fn tray_icon_rgba() -> Result<Icon, String> {
    let width = 16;
    let height = 16;
    let mut rgba = Vec::with_capacity(width * height * 4);

    for y in 0..height {
        for x in 0..width {
            let border = x == 0 || y == 0 || x == width - 1 || y == height - 1;
            let diagonal = x == y || x + y == width - 1;
            let (r, g, b, a) = if border {
                (36, 42, 54, 255)
            } else if diagonal {
                (220, 55, 65, 255)
            } else {
                (245, 245, 245, 255)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    Icon::from_rgba(rgba, width as u32, height as u32)
        .map_err(|error| format!("Could not create tray icon image: {error}"))
}

enum BackgroundMessage {
    Checked { updates_found: usize },
    Error(String),
}

struct BackgroundChecker {
    enabled: Arc<AtomicBool>,
    path: Arc<Mutex<Option<PathBuf>>>,
}

impl BackgroundChecker {
    fn start(sender: Sender<BackgroundMessage>, path: Option<PathBuf>, enabled: bool) -> Self {
        let checker = Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            path: Arc::new(Mutex::new(path)),
        };

        let thread_enabled = Arc::clone(&checker.enabled);
        let thread_path = Arc::clone(&checker.path);
        thread::spawn(move || {
            loop {
                if thread_enabled.load(Ordering::Relaxed) && daily_check_due() {
                    let path = thread_path.lock().ok().and_then(|path| path.clone());
                    if let Some(path) = path {
                        match check_for_updates_at_path(&path) {
                            Ok(updates_found) => {
                                save_last_background_check(now_epoch_secs());
                                if updates_found > 0 {
                                    send_update_notification(updates_found);
                                }
                                let _ = sender.send(BackgroundMessage::Checked { updates_found });
                            }
                            Err(error) => {
                                let _ = sender.send(BackgroundMessage::Error(error));
                            }
                        }
                    }
                }

                thread::sleep(Duration::from_secs(60));
            }
        });

        checker
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn set_path(&self, path: Option<PathBuf>) {
        if let Ok(mut current_path) = self.path.lock() {
            *current_path = path;
        }
    }
}

fn online_error_dialog(ui: &mut egui::Ui, error: &str) -> OnlineErrorAction {
    let mut action = OnlineErrorAction::None;

    egui::Window::new("Network error")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ui.ctx(), |ui| {
            ui.set_width(520.0);
            ui.heading("Could not fetch online app list");
            ui.add_space(6.0);
            ui.label("RuTracker may be temporarily unavailable. The local Adobe scan was kept, so you can retry the network request.");
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(210, 95, 55), error);
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Retry").clicked() {
                    action = OnlineErrorAction::Retry;
                }
                if ui.button("Change folder").clicked() {
                    action = OnlineErrorAction::ChangeFolder;
                }
            });
        });

    action
}

fn style_ui(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size = egui::vec2(84.0, 34.0);
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(24.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(15.0, egui::FontFamily::Proportional),
    );
    style.visuals.widgets.inactive.corner_radius = 4.into();
    style.visuals.widgets.hovered.corner_radius = 4.into();
    style.visuals.widgets.active.corner_radius = 4.into();
    ctx.set_global_style(style);
}

fn section_header(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.heading(title);
    ui.add_space(4.0);
}

fn updates_header(ui: &mut egui::Ui) {
    table_row(ui, 34.0, true, |ui, widths| {
        table_label(ui, widths[0], egui::RichText::new("Application").strong());
        table_label(ui, widths[1], egui::RichText::new("Installed").strong());
        table_label(ui, widths[2], egui::RichText::new("Newest").strong());
        table_label(ui, widths[3], egui::RichText::new("Magnet").strong());
    });
}

fn update_row(ui: &mut egui::Ui, local_app: &LocalFoundApp, index: usize) {
    table_row(ui, 42.0, index % 2 == 0, |ui, widths| {
        table_label(ui, widths[0], egui::RichText::new(&local_app.name));
        table_label(ui, widths[1], egui::RichText::new(&local_app.version));
        if let Some(newest_online) = &local_app.newest_online {
            table_label(
                ui,
                widths[2],
                egui::RichText::new(&newest_online.version)
                    .color(egui::Color32::from_rgb(205, 70, 55))
                    .strong(),
            );
            action_buttons(ui, widths[3], &newest_online.magnet);
        } else {
            table_label(
                ui,
                widths[2],
                egui::RichText::new("Up to date")
                    .color(egui::Color32::from_rgb(60, 135, 85))
                    .strong(),
            );
            table_label(ui, widths[3], egui::RichText::new("-"));
        }
    });
}

fn online_apps_header(ui: &mut egui::Ui) {
    table_row(ui, 34.0, false, |ui, widths| {
        table_label(ui, widths[0], egui::RichText::new("Application").strong());
        table_label(ui, widths[1], egui::RichText::new("Version").strong());
        table_label(
            ui,
            widths[2] + widths[3],
            egui::RichText::new("Magnet").strong(),
        );
    });
}

fn online_app_row(ui: &mut egui::Ui, online_app: &OnlineFoundApp, index: usize) {
    table_row(ui, 42.0, index % 2 == 0, |ui, widths| {
        table_label(ui, widths[0], egui::RichText::new(&online_app.name));
        table_label(ui, widths[1], egui::RichText::new(&online_app.version));
        action_buttons(ui, widths[2] + widths[3], &online_app.magnet);
    });
}

fn table_row(
    ui: &mut egui::Ui,
    height: f32,
    striped: bool,
    add_contents: impl FnOnce(&mut egui::Ui, [f32; 4]),
) {
    let width = ui.available_width();
    let usable_width = (width - ui.spacing().item_spacing.x * 3.0).max(0.0);
    let app_width = (usable_width * 0.28).clamp(180.0, 300.0);
    let installed_width = 105.0;
    let newest_width = 105.0;
    let magnet_width = (usable_width - app_width - installed_width - newest_width).max(240.0);
    let widths = [app_width, installed_width, newest_width, magnet_width];
    let frame = if striped {
        egui::Frame::new().fill(ui.visuals().faint_bg_color)
    } else {
        egui::Frame::new()
    };

    frame.show(ui, |ui| {
        ui.set_width(width);
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| add_contents(ui, widths),
        );
    });
}

fn table_label(ui: &mut egui::Ui, width: f32, text: egui::RichText) {
    ui.add_sized([width, 26.0], egui::Label::new(text).truncate());
}

fn action_buttons(ui: &mut egui::Ui, width: f32, magnet: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 34.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let button_width = ui.spacing().interact_size.x * 2.0 + ui.spacing().item_spacing.x;
            let url_width = (width - button_width - ui.spacing().item_spacing.x).max(80.0);
            ui.add_sized([url_width, 34.0], egui::Label::new(magnet).truncate())
                .on_hover_text(magnet);
            ui.add_sized(
                [ui.spacing().interact_size.x, 34.0],
                egui::Hyperlink::from_label_and_url("Open", magnet),
            );
            if ui
                .add_sized(
                    [ui.spacing().interact_size.x, 34.0],
                    egui::Button::new("Copy"),
                )
                .on_hover_text("Copy magnet link")
                .clicked()
            {
                ui.ctx().copy_text(magnet.to_owned());
            }
        },
    );
}

fn check_for_updates_at_path(path: &Path) -> Result<usize, String> {
    let mut local_apps = find_local_programs(path);
    if local_apps.is_empty() {
        return Err(format!(
            "No installed Adobe products were found under {}.",
            path.display()
        ));
    }

    let mut online_apps = find_online_programs()?;
    compare_versions(&mut local_apps, &mut online_apps);
    Ok(local_apps
        .iter()
        .filter(|local_app| local_app.newest_online.is_some())
        .count())
}

fn send_update_notification(updates_found: usize) {
    let _ = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Adobe updates found")
        .text1(&format!(
            "{updates_found} installed Adobe app(s) have updates."
        ))
        .text2("Open M0nkrus Adobe Tracker for magnet links.")
        .duration(ToastDuration::Short)
        .show();
}

fn daily_check_due() -> bool {
    let Some(last_check) = last_background_check() else {
        return true;
    };
    now_epoch_secs().saturating_sub(last_check) >= DAILY_CHECK_INTERVAL_SECS
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn settings_key() -> Option<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(SETTINGS_KEY)
        .ok()
        .map(|(key, _)| key)
}

fn background_updates_enabled() -> bool {
    settings_key()
        .and_then(|key| key.get_value::<u32, _>(BACKGROUND_ENABLED_VALUE).ok())
        .is_some_and(|enabled| enabled != 0)
}

fn set_background_updates_enabled(enabled: bool) {
    if let Some(key) = settings_key() {
        let _ = key.set_value(BACKGROUND_ENABLED_VALUE, &(enabled as u32));
    }
}

fn last_background_check() -> Option<u64> {
    settings_key()
        .and_then(|key| key.get_value::<String, _>(LAST_BACKGROUND_CHECK_VALUE).ok())
        .and_then(|value| value.parse().ok())
}

fn save_last_background_check(timestamp: u64) {
    if let Some(key) = settings_key() {
        let _ = key.set_value(LAST_BACKGROUND_CHECK_VALUE, &timestamp.to_string());
    }
}

fn start_on_boot_enabled() -> bool {
    startup_key(KEY_READ)
        .and_then(|key| key.get_value::<String, _>(RUN_VALUE_NAME))
        .is_ok()
}

fn set_start_on_boot(enabled: bool) -> Result<(), String> {
    let key = startup_key(KEY_READ | KEY_SET_VALUE)
        .map_err(|error| format!("Could not open startup registry key: {error}"))?;
    if enabled {
        let exe = std::env::current_exe()
            .map_err(|error| format!("Could not read current executable path: {error}"))?;
        key.set_value(RUN_VALUE_NAME, &format!("\"{}\"", exe.display()))
            .map_err(|error| format!("Could not write startup registry value: {error}"))?;
    } else {
        match key.delete_value(RUN_VALUE_NAME) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("Could not remove startup registry value: {error}"));
            }
        }
    }

    Ok(())
}

fn startup_key(flags: u32) -> std::io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", flags)
}

fn compare_versions(installed_apps: &mut [LocalFoundApp], online_apps: &mut [OnlineFoundApp]) {
    for local_app in installed_apps.iter_mut() {
        for online_app in online_apps.iter() {
            if online_app.name.contains(&local_app.name) {
                if local_app.newest_online.is_none() {
                    if Version::from(&local_app.version) < Version::from(&online_app.version) {
                        local_app.newest_online = Some(online_app.clone());
                    }
                } else if Version::from(&local_app.newest_online.as_ref().unwrap().version)
                    < Version::from(&online_app.version)
                {
                    local_app.newest_online = Some(online_app.clone());
                }
            }
        }
    }
}

fn find_online_programs() -> Result<Vec<OnlineFoundApp>, String> {
    println!("Looking for online apps");
    let mut online_apps = Vec::new();

    let client = Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Failed creating HTTP client: {error}"))?;
    let html = client
        .get(TRACKER_URL)
        .send()
        .map_err(|error| format!("Failed downloading tracker page: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Tracker returned an error status: {error}"))?
        .text()
        .map_err(|error| format!("Failed reading tracker page: {error}"))?;

    let document = Html::parse_document(&html);
    let row_selector = Selector::parse("tr.hl-tr")
        .map_err(|error| format!("Failed creating row selector: {error}"))?;
    let title_selector = Selector::parse("a.tLink")
        .map_err(|error| format!("Failed creating title selector: {error}"))?;
    let magnet_selector = Selector::parse(r#"a[href^="magnet:?"]"#)
        .map_err(|error| format!("Failed creating magnet selector: {error}"))?;

    for row in document.select(&row_selector) {
        let Some(title) = row.select(&title_selector).next() else {
            continue;
        };

        let title_text = title
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .replace('\u{a0}', " ");
        let title_text = title_text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !title_text.to_ascii_lowercase().contains("adobe") {
            continue;
        }

        let Some(magnet) = row
            .select(&magnet_selector)
            .next()
            .and_then(|element| element.value().attr("href"))
        else {
            continue;
        };

        if let Some((name, version)) = parse_online_title(&title_text) {
            let online_app = OnlineFoundApp {
                name,
                magnet: magnet.to_owned(),
                version,
            };
            println!(
                "App: {}\nVersion: {}\nMagnet:{}\n",
                &online_app.name, &online_app.version, &online_app.magnet
            );
            online_apps.push(online_app.clone());
        }
    }
    if online_apps.is_empty() {
        return Err("The tracker page loaded, but no Adobe magnet entries were found.".to_owned());
    }

    Ok(online_apps)
}

fn parse_online_title(title: &str) -> Option<(String, String)> {
    let title = title.trim();
    let adobe_start = title.find("Adobe")?;
    let title = &title[adobe_start..];

    if let Some(version_start) = title.find("(v") {
        let version_end = title[version_start + 2..]
            .find(')')
            .map(|end| version_start + 2 + end)
            .unwrap_or(title.len());
        let version = title[version_start + 2..version_end].trim().to_owned();
        let name = title[..version_start].trim().to_owned();
        return Some((name, version));
    }

    for token in title.split_whitespace() {
        if token.len() > 1
            && token.starts_with('v')
            && token[1..]
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_digit())
        {
            let version = token[1..]
                .trim_matches(|c: char| !c.is_ascii_digit() && c != '.')
                .to_owned();
            let name_end = title.find(token)?;
            let name = title[..name_end].trim().to_owned();
            return Some((name, version));
        }
    }

    None
}

fn find_local_programs(path: &Path) -> Vec<LocalFoundApp> {
    let mut apps = Vec::new();

    for root in uninstall_roots() {
        collect_local_programs_from_registry(&root, path, &mut apps);
    }

    apps.sort_by(|left, right| left.name.cmp(&right.name));
    apps.dedup_by(|left, right| left.name == right.name && left.version == right.version);
    apps
}

fn uninstall_roots() -> Vec<RegKey> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let uninstall_key = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

    [
        hklm.open_subkey_with_flags(uninstall_key, KEY_READ | KEY_WOW64_64KEY),
        hklm.open_subkey_with_flags(uninstall_key, KEY_READ | KEY_WOW64_32KEY),
        hkcu.open_subkey_with_flags(uninstall_key, KEY_READ | KEY_WOW64_64KEY),
        hkcu.open_subkey_with_flags(uninstall_key, KEY_READ | KEY_WOW64_32KEY),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_local_programs_from_registry(
    uninstall_root: &RegKey,
    adobe_folder: &Path,
    apps: &mut Vec<LocalFoundApp>,
) {
    for subkey_name in uninstall_root.enum_keys().flatten() {
        let Ok(app_key) = uninstall_root.open_subkey_with_flags(subkey_name, KEY_READ) else {
            continue;
        };

        let Ok(display_name): Result<String, _> = app_key.get_value("DisplayName") else {
            continue;
        };
        if !display_name.to_ascii_lowercase().contains("adobe") {
            continue;
        }

        let Ok(install_location): Result<String, _> = app_key.get_value("InstallLocation") else {
            continue;
        };
        let install_location = PathBuf::from(install_location);
        if !is_path_inside(&install_location, adobe_folder) {
            continue;
        }

        let Ok(version): Result<String, _> = app_key.get_value("DisplayVersion") else {
            continue;
        };
        let name = normalize_local_app_name(&display_name);
        println!(
            "App: {}, Version: {}, InstallLocation: {}",
            &name,
            &version,
            install_location.display()
        );
        apps.push(LocalFoundApp {
            version,
            name,
            newest_online: None,
        });
    }
}

fn is_path_inside(path: &Path, parent: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(parent) = parent.canonicalize() else {
        return false;
    };
    path.starts_with(parent)
}

fn normalize_local_app_name(display_name: &str) -> String {
    let words: Vec<&str> = display_name.split_whitespace().collect();
    let version_word_index = words
        .iter()
        .position(|word| {
            word.chars()
                .next()
                .is_some_and(|char| char.is_ascii_digit())
        })
        .unwrap_or(words.len());
    let mut name = words[..version_word_index].join(" ");
    if let Some(parenthetical_start) = name.find(" (") {
        name.truncate(parenthetical_start);
    }
    name.trim().to_owned()
}

#[derive(Clone)]
pub struct OnlineFoundApp {
    pub version: String,
    pub name: String,
    pub magnet: String,
}

pub struct LocalFoundApp {
    pub version: String,
    pub name: String,
    pub newest_online: Option<OnlineFoundApp>,
}
