// Kosmik Layouts tray app. No window of its own: the control page opens in the default browser, so
// nothing heavy stays loaded while it sits in the tray.
//   --autostart  started with Windows: don't open the page
//   --headless   no tray, just the server (development and checks)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod updates;

use kosmik_layouts::controller::Controller;
use kosmik_layouts::hotkeys::{self, Hotkeys};
use kosmik_layouts::{App, Options, StartError, log, start};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, Wry};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

#[derive(Clone, Copy)]
struct Text {
    layouts: &'static str,
    automatic: &'static str,
    no_layouts: &'static str,
    hint_lock: &'static str,
    hint_next: &'static str,
    open: &'static str,
    autostart: &'static str,
    quit: &'static str,
    update: &'static str,
    updating: &'static str,
    busy: &'static str,
    failed: &'static str,
}

fn spanish() -> bool {
    // LANG_SPANISH is primary language 0x0a.
    (unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() } & 0x3ff) == 0x0a
}

fn text() -> Text {
    if spanish() {
        Text {
            layouts: "Elegir layout",
            automatic: "Automático (según el juego)",
            no_layouts: "Todavía no hay layouts",
            hint_lock: "{key}: fijar / volver a automático",
            hint_next: "{key}: siguiente layout",
            open: "Abrir Kosmik Layouts",
            autostart: "Iniciar con Windows",
            quit: "Salir",
            update: "Instalar la actualización {version}",
            updating: "Actualizando…",
            busy: "Kosmik Layouts no pudo arrancar: el puerto {port} está en uso.\n\n¿Ya está abierto Kosmik Layouts? Fijate en la bandeja del sistema.",
            failed: "Kosmik Layouts no pudo arrancar:\n\n{error}",
        }
    } else {
        Text {
            layouts: "Choose layout",
            automatic: "Automatic (follows your games)",
            no_layouts: "No layouts yet",
            hint_lock: "{key}: lock / back to automatic",
            hint_next: "{key}: next layout",
            open: "Open Kosmik Layouts",
            autostart: "Start with Windows",
            quit: "Quit",
            update: "Install update {version}",
            updating: "Updating…",
            busy: "Kosmik Layouts couldn't start: port {port} is in use.\n\nIs Kosmik Layouts already running? Check the system tray.",
            failed: "Kosmik Layouts couldn't start:\n\n{error}",
        }
    }
}

fn obs_label(state: &str) -> &'static str {
    match (state, spanish()) {
        ("connected", false) => "OBS: connected",
        ("connected", true) => "OBS: conectado",
        ("connecting", false) => "OBS: connecting…",
        ("connecting", true) => "OBS: conectando…",
        ("auth_failed", false) => "OBS rejected the password: open the page",
        ("auth_failed", true) => "OBS rechazó la contraseña: abrí la página",
        (_, false) => "OBS isn't connected",
        (_, true) => "OBS no está conectado",
    }
}

fn layout_label(name: Option<String>, locked: bool) -> String {
    let lock = match (locked, spanish()) {
        (false, _) => "",
        (true, false) => " (locked)",
        (true, true) => " (fijado)",
    };
    match (name, spanish()) {
        (Some(n), false) => format!("Layout on screen: {}{lock}", menu_text(&n)),
        (Some(n), true) => format!("Layout en pantalla: {}{lock}", menu_text(&n)),
        (None, false) => "Layout on screen: none yet".into(),
        (None, true) => "Layout en pantalla: todavía ninguno".into(),
    }
}

/// Windows menus treat "&" as a shortcut marker; "&&" shows one.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// "Choose layout": Automatic plus one line per layout, the locked one ticked, and the hotkeys.
struct LayoutMenu {
    sub: Submenu<Wry>,
    shown: Option<(Vec<(String, String)>, Vec<String>)>,
    checks: Vec<(Option<String>, CheckMenuItem<Wry>)>,
}

impl LayoutMenu {
    /// `hints`: one line per hotkey that is set ("Ctrl+Alt+Shift+L: lock / back to automatic").
    fn refresh(&mut self, app: &AppHandle, layouts: Vec<(String, String)>, hints: Vec<String>, locked: Option<&str>, t: &Text) -> tauri::Result<()> {
        let key = (layouts, hints);
        if self.shown.as_ref() != Some(&key) {
            let (layouts, hints) = &key;
            for item in self.sub.items()? {
                self.sub.remove(&item)?;
            }
            self.checks.clear();
            let auto = CheckMenuItem::with_id(app, "lock:", t.automatic, true, false, None::<&str>)?;
            self.sub.append(&auto)?;
            self.checks.push((None, auto));
            self.sub.append(&PredefinedMenuItem::separator(app)?)?;
            if layouts.is_empty() {
                self.sub.append(&MenuItem::with_id(app, "no-layouts", t.no_layouts, false, None::<&str>)?)?;
            }
            for (id, name) in layouts {
                let c = CheckMenuItem::with_id(app, format!("lock:{id}"), menu_text(name), true, false, None::<&str>)?;
                self.sub.append(&c)?;
                self.checks.push((Some(id.clone()), c));
            }
            if !hints.is_empty() {
                self.sub.append(&PredefinedMenuItem::separator(app)?)?;
            }
            for (i, hint) in hints.iter().enumerate() {
                self.sub.append(&MenuItem::with_id(app, format!("hint-{i}"), hint, false, None::<&str>)?)?;
            }
            self.shown = Some(key);
        }
        // Set every time: Windows ticks or unticks a clicked item by itself.
        for (id, c) in &self.checks {
            c.set_checked(id.as_deref() == locked)?;
        }
        Ok(())
    }
}

/// Tray lines for the hotkeys that are set.
fn hotkey_hints(keys: &[(String, String)], t: &Text) -> Vec<String> {
    keys.iter()
        .filter(|(_, k)| !k.is_empty())
        .filter_map(|(id, k)| match id.as_str() {
            "lock" => Some(t.hint_lock.replace("{key}", k)),
            "next" => Some(t.hint_next.replace("{key}", k)),
            _ => None,
        })
        .collect()
}

/// Hotkeys registered with Windows right now, and what each one does.
type Bound = Arc<Mutex<Vec<(Shortcut, String)>>>;

/// (Re)registers the chosen hotkeys, telling the page about any Windows refuses (another program has
/// that combination). Runs on the main thread, where the plugin's hotkey window lives.
fn bind_hotkeys(app: &AppHandle, hk: &Hotkeys, bound: &Bound) {
    let gs = app.global_shortcut();
    let mut b = bound.lock().unwrap_or_else(|e| e.into_inner());
    for (s, _) in b.drain(..) {
        let _ = gs.unregister(s);
    }
    if hk.paused() {
        return; // the page is waiting for a new combination
    }
    for (id, key) in hk.keys() {
        if key.is_empty() {
            hk.set_error(&id, None);
            continue;
        }
        let Ok(s) = hotkeys::plugin_form(&key).parse::<Shortcut>() else {
            hk.set_error(&id, Some("unsupported_key"));
            continue;
        };
        match gs.register(s) {
            Ok(()) => {
                hk.set_error(&id, None);
                b.push((s, id));
            }
            Err(e) => {
                log::line(format!("hotkey {key} unavailable (another program uses it?): {e}"));
                hk.set_error(&id, Some("taken"));
            }
        }
    }
}

/// Registers now, and again whenever the page changes a key or pauses them.
fn watch_hotkeys(app: AppHandle, hk: Arc<Hotkeys>, bound: Bound) {
    bind_hotkeys(&app, &hk, &bound);
    let (mut keys, mut paused) = (hk.keys_watch(), hk.paused_watch());
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                r = keys.changed() => if r.is_err() { return },
                r = paused.changed() => if r.is_err() { return },
            }
            keys.borrow_and_update();
            paused.borrow_and_update();
            let (a, h, b) = (app.clone(), hk.clone(), bound.clone());
            let _ = app.run_on_main_thread(move || bind_hotkeys(&a, &h, &b));
        }
    });
}

/// What a hotkey does.
fn run_hotkey(ctl: &Arc<Controller>, action: &str) {
    let c = ctl.clone();
    match action {
        "lock" => drop(tauri::async_runtime::spawn(async move { c.toggle_lock().await })),
        "next" => drop(tauri::async_runtime::spawn(async move { c.lock_next().await })),
        _ => {}
    }
}

fn message_box(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (body, title) = (wide(msg), wide("Kosmik Layouts"));
    unsafe { MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING) };
}

fn start_error(e: &StartError, port: u16) -> String {
    let t = text();
    match e {
        StartError::PortBusy(p) => t.busy.replace("{port}", &p.to_string()),
        StartError::Other(m) => t.failed.replace("{error}", m).replace("{port}", &port.to_string()),
    }
}

fn open_page(app: &AppHandle) {
    if let Some(url) = app.try_state::<String>() {
        let _ = app.opener().open_url(format!("{}/", url.inner()), None::<&str>);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    // Two worker threads are plenty for one OBS connection and a page that polls.
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime")
}

fn headless(opts: Options) {
    let rt = runtime();
    rt.block_on(async {
        let port = opts.port;
        match start(opts).await {
            Ok(_app) => {
                let _ = tokio::signal::ctrl_c().await;
            }
            Err(e) => {
                log::line(start_error(&e, port));
                std::process::exit(1);
            }
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opts = Options::from_env();
    if args.iter().any(|a| a == "--headless") {
        return headless(opts);
    }
    let autostarted = args.iter().any(|a| a == "--autostart");

    let rt = runtime();
    tauri::async_runtime::set(rt.handle().clone());
    let port = opts.port;
    let update_check = opts.update_check;

    let t = text();
    let app = tauri::Builder::default()
        // A second launch (Start menu, desktop shortcut) just opens the page in the running copy.
        // Plugins are set up before `setup`, so a second copy exits here, before touching the port.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| open_page(app)))
        // The entry is named after productName; the uninstall hook (windows/hooks.nsh) removes it.
        .plugin(tauri_plugin_autostart::Builder::new().arg("--autostart").build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let started: App = match tauri::async_runtime::block_on(start(opts)) {
                Ok(app) => app,
                Err(e) => {
                    log::line(start_error(&e, port));
                    message_box(&start_error(&e, port));
                    std::process::exit(1);
                }
            };
            app.manage(started.url.clone());
            app.manage(started.server.clone());

            let launcher = app.autolaunch();
            // Enable autostart on first run so layouts follow your games whenever OBS is open.
            // If already enabled, refresh the program path; if disabled, leave it off.
            let marker = started.data_dir.join("app-first-run.done");
            if !marker.exists() || launcher.is_enabled().unwrap_or(false) {
                let _ = launcher.enable();
            }
            let _ = std::fs::write(&marker, "");
            let ctl = started.controller.clone();
            let status = MenuItem::with_id(app, "status", obs_label(&ctl.obs.status().state), false, None::<&str>)?;
            let layout = MenuItem::with_id(app, "layout", layout_label(None, false), false, None::<&str>)?;
            let layouts = Submenu::with_id(app, "layouts", t.layouts, true)?;
            let open = MenuItem::with_id(app, "open", t.open, true, None::<&str>)?;
            let auto = CheckMenuItem::with_id(app, "autostart", t.autostart, true, launcher.is_enabled().unwrap_or(false), None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", t.quit, true, None::<&str>)?;
            let sep = || PredefinedMenuItem::separator(app);
            let menu = Menu::with_items(app, &[&open, &sep()?, &status, &layout, &layouts, &sep()?, &auto, &quit])?;
            // An "Install update" line joins the top of this menu when there is one.
            let updates = updates::Updates::new(
                app.handle().clone(),
                started.server.clone(),
                menu.clone(),
                updates::Labels { install: t.update, installing: t.updating },
                started.data_dir.clone(),
            )?;
            if update_check {
                updates.watch();
            }
            // Restarted by an update: the page that started it is still open and reloads by itself.
            let updated = std::fs::remove_file(started.data_dir.join(updates::UPDATED_FLAG)).is_ok();

            // Hotkeys (also for a Stream Deck's hotkey button): Ctrl+Alt+Shift+L / N unless changed on the page.
            let bound: Bound = Arc::new(Mutex::new(Vec::new()));
            let (hk_ctl, b) = (ctl.clone(), bound.clone());
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |_app, shortcut, event| {
                        if event.state() != ShortcutState::Pressed {
                            return;
                        }
                        let action = b.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| s == shortcut).map(|(_, a)| a.clone());
                        if let Some(a) = action {
                            run_hotkey(&hk_ctl, &a);
                        }
                    })
                    .build(),
            )?;
            watch_hotkeys(app.handle().clone(), started.hotkeys.clone(), bound);

            let menu_ctl = ctl.clone();
            let tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("no app icon")?)
                .tooltip("Kosmik Layouts")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        open_page(tray.app_handle());
                    }
                })
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => open_page(app),
                    "update" => updates.install(),
                    "autostart" => {
                        let l = app.autolaunch();
                        let on = l.is_enabled().unwrap_or(false);
                        let _ = if on { l.disable() } else { l.enable() };
                        let _ = auto.set_checked(l.is_enabled().unwrap_or(!on));
                    }
                    "quit" => app.exit(0),
                    id if id.starts_with("lock:") => {
                        let target = id["lock:".len()..].to_string();
                        let c = menu_ctl.clone();
                        tauri::async_runtime::spawn(async move { c.set_lock((!target.is_empty()).then_some(target.as_str())).await });
                    }
                    _ => {}
                })
                .build(app)?;

            // Keep the status lines, the layout list and the tooltip in step with OBS, the layouts and what's on screen.
            let mut obs_watch = ctl.obs.status_watch();
            let mut changes = ctl.changes();
            let mut saved = ctl.store.revisions();
            let mut keys = started.hotkeys.keys_watch();
            let handle = app.handle().clone();
            let mut layout_menu = LayoutMenu { sub: layouts, shown: None, checks: Vec::new() };
            tauri::async_runtime::spawn(async move {
                loop {
                    let obs_text = obs_label(&obs_watch.borrow_and_update().state);
                    changes.borrow_and_update();
                    saved.borrow_and_update();
                    let hints = hotkey_hints(&keys.borrow_and_update(), &t);
                    let name = ctl.active().and_then(|id| ctl.profile(&id)).and_then(|p| p["name"].as_str().map(String::from));
                    let locked = ctl.lock_id();
                    let list = ctl.store.profiles().iter().filter_map(|p| Some((p["id"].as_str()?.to_string(), p["name"].as_str().unwrap_or("").to_string()))).collect();
                    if let Err(e) = layout_menu.refresh(&handle, list, hints, locked.as_deref(), &t) {
                        log::line(format!("tray menu: {e}"));
                    }
                    let _ = status.set_text(obs_text);
                    let _ = layout.set_text(layout_label(name.clone(), locked.is_some()));
                    let tip = match name {
                        Some(n) => format!("Kosmik Layouts · {obs_text} · {n}"),
                        None => format!("Kosmik Layouts · {obs_text}"),
                    };
                    let _ = tray.set_tooltip(Some(tip));
                    tokio::select! {
                        r = obs_watch.changed() => if r.is_err() { return },
                        r = changes.changed() => if r.is_err() { return },
                        r = saved.changed() => if r.is_err() { return },
                        r = keys.changed() => if r.is_err() { return },
                    }
                }
            });

            if !autostarted && !updated {
                open_page(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(e) => {
            log::line(format!("tray app failed: {e}"));
            message_box(&start_error(&StartError::Other(e.to_string()), port));
            std::process::exit(1);
        }
    };
    app.run(|_app, event| {
        // There are no windows, so only an explicit Quit (exit with a code) ends the app.
        if let RunEvent::ExitRequested { api, code: None, .. } = event {
            api.prevent_exit();
        }
    });
}
