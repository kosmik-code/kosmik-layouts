// Kosmik Layouts tray app. No window of its own: the control page opens in the default browser, so
// nothing heavy stays loaded while it sits in the tray.
//   --autostart  started with Windows: don't open the page
//   --headless   no tray, just the server (development and checks)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod updates;

use kosmik_layouts::{App, Options, StartError, log, start};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;

struct Text {
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
            open: "Abrir Kosmik Layouts",
            autostart: "Iniciar con Windows",
            quit: "Salir",
            update: "Instalar la actualización {version}",
            updating: "Actualizando…",
            busy: "Kosmik Layouts no pudo arrancar: el puerto {port} está en uso.\n\n¿Ya está abierto Kosmik Layouts? Fijate en la bandeja del sistema, o cerrá la versión de terminal (npm start).",
            failed: "Kosmik Layouts no pudo arrancar:\n\n{error}",
        }
    } else {
        Text {
            open: "Open Kosmik Layouts",
            autostart: "Start with Windows",
            quit: "Quit",
            update: "Install update {version}",
            updating: "Updating…",
            busy: "Kosmik Layouts couldn't start: port {port} is in use.\n\nIs Kosmik Layouts already running? Check the system tray, or close the terminal version (npm start).",
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

fn layout_label(name: Option<String>) -> String {
    match (name, spanish()) {
        (Some(n), false) => format!("Layout on screen: {n}"),
        (Some(n), true) => format!("Layout en pantalla: {n}"),
        (None, false) => "Layout on screen: none yet".into(),
        (None, true) => "Layout en pantalla: todavía ninguno".into(),
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
            // First run of the app (even with layouts copied over from the Node.js version): start with
            // Windows, so layouts follow your games whenever OBS is open. Already on: write it again, so
            // it follows the program if it was installed somewhere else. Turned off stays off.
            let marker = started.data_dir.join("app-first-run.done");
            if !marker.exists() || launcher.is_enabled().unwrap_or(false) {
                let _ = launcher.enable();
            }
            let _ = std::fs::write(&marker, "");
            let ctl = started.controller.clone();
            let status = MenuItem::with_id(app, "status", obs_label(&ctl.obs.status().state), false, None::<&str>)?;
            let layout = MenuItem::with_id(app, "layout", layout_label(None), false, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", t.open, true, None::<&str>)?;
            let auto = CheckMenuItem::with_id(app, "autostart", t.autostart, true, launcher.is_enabled().unwrap_or(false), None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", t.quit, true, None::<&str>)?;
            let sep = || PredefinedMenuItem::separator(app);
            let menu = Menu::with_items(app, &[&open, &sep()?, &status, &layout, &sep()?, &auto, &quit])?;
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
                    _ => {}
                })
                .build(app)?;

            // Keep the status lines and tooltip in step with OBS and the layout on screen.
            let mut obs_watch = ctl.obs.status_watch();
            let mut changes = ctl.changes();
            tauri::async_runtime::spawn(async move {
                loop {
                    let obs_text = obs_label(&obs_watch.borrow_and_update().state);
                    changes.borrow_and_update();
                    let name = ctl.active().and_then(|id| ctl.profile(&id)).and_then(|p| p["name"].as_str().map(String::from));
                    let _ = status.set_text(obs_text);
                    let _ = layout.set_text(layout_label(name.clone()));
                    let tip = match name {
                        Some(n) => format!("Kosmik Layouts · {obs_text} · {n}"),
                        None => format!("Kosmik Layouts · {obs_text}"),
                    };
                    let _ = tray.set_tooltip(Some(tip));
                    tokio::select! {
                        r = obs_watch.changed() => if r.is_err() { return },
                        r = changes.changed() => if r.is_err() { return },
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
