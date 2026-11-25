//! Kosmik Layouts backend: the OBS WebSocket client, game detection, the controller that keeps OBS
//! on the right layout, and the local web server for the control page. The tray app (main.rs) is a
//! thin shell around `start`.

pub mod connection;
pub mod controller;
pub mod games;
pub mod js;
pub mod layout;
pub mod log;
pub mod obs;
pub mod server;
pub mod store;

#[cfg(test)]
pub mod fake_obs;

use include_dir::{Dir, include_dir};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The control page, built into the program.
pub static PUBLIC: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../public");

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_PORT: u16 = 7430;

pub struct Options {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Serve the page from this folder instead of the built-in copy (development).
    pub public_dir: Option<PathBuf>,
    /// KL_OBS_URL / KL_OBS_PASSWORD: fixed OBS address and password for this run.
    pub obs_url: Option<String>,
    pub obs_password: Option<String>,
    pub update_check: bool,
}

/// %APPDATA%\Kosmik Layouts
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Kosmik Layouts")
}

impl Options {
    /// KL_PORT, KL_DATA_DIR, KL_OBS_URL, KL_OBS_PASSWORD like the Node version, plus KL_PUBLIC_DIR
    /// and KL_UPDATE_CHECK=0.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Options {
            port: var("KL_PORT").and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT),
            data_dir: var("KL_DATA_DIR").map(PathBuf::from).unwrap_or_else(default_data_dir),
            public_dir: var("KL_PUBLIC_DIR").map(PathBuf::from),
            obs_url: var("KL_OBS_URL"),
            // Set but empty still counts: "no password".
            obs_password: std::env::var("KL_OBS_PASSWORD").ok(),
            update_check: var("KL_UPDATE_CHECK").as_deref() != Some("0"),
        }
    }
}

pub struct App {
    pub url: String,
    pub data_dir: PathBuf,
    pub controller: Arc<controller::Controller>,
    pub server: Arc<server::ServerHandle>,
}

#[derive(Debug)]
pub enum StartError {
    PortBusy(u16),
    Other(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::PortBusy(p) => write!(f, "port {p} is busy"),
            StartError::Other(m) => f.write_str(m),
        }
    }
}

/// Starts everything on the current Tokio runtime.
pub async fn start(opts: Options) -> Result<App, StartError> {
    let other = |e: std::io::Error| StartError::Other(e.to_string());
    std::fs::create_dir_all(&opts.data_dir).map_err(other)?;
    log::to_file(&opts.data_dir.join("kosmik-layouts.log"));
    log::line(format!("Kosmik Layouts {VERSION}, data in {}", opts.data_dir.display()));

    // Bind first: if another copy is running, stop before touching OBS.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port)).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse || e.raw_os_error() == Some(10013) { StartError::PortBusy(opts.port) } else { other(e) }
    })?;

    let store = Arc::new(store::Store::load(&opts.data_dir).map_err(other)?);

    // Address and password: KL_OBS_URL / KL_OBS_PASSWORD, else what was typed on the page, else OBS's own settings.
    let obs = obs::ObsClient::new(Duration::from_secs(3), Duration::from_secs(5));
    let app_data = std::env::var_os("APPDATA").map(PathBuf::from);
    let read_settings: connection::ReadSettings = Arc::new(move || obs::read_obs_settings(app_data.as_deref()));
    let connection = connection::Connection::load(obs.clone(), &opts.data_dir, opts.obs_url.clone(), opts.obs_password.clone(), read_settings);
    let s = store.clone();
    let watcher = games::GameWatcher::new(Arc::new(move || s.profiles()), Arc::new(games::list_processes), Duration::from_secs(3));
    let controller = controller::Controller::new(obs.clone(), store, Some(&watcher));
    controller.set_connection(connection.clone());
    connection.watch();

    let public = match opts.public_dir {
        Some(dir) => server::PublicFiles::Disk(dir),
        None => server::PublicFiles::Embedded(&PUBLIC),
    };
    let server = Arc::new(
        server::start_server(
            listener,
            server::ServerOptions { controller: controller.clone(), connection: Some(connection.clone()), list_apps: Arc::new(games::list_windowed_apps), public },
        )
        .await
        .map_err(other)?,
    );

    let mut status = obs.status_watch();
    tokio::spawn(async move {
        while status.changed().await.is_ok() {
            let s = status.borrow_and_update().clone();
            log::line(format!("[obs] {}{}", s.state, if s.message.is_empty() { String::new() } else { format!(": {}", s.message) }));
        }
    });
    if connection.source() == "auto" && connection.detected().is_some_and(|d| !d.enabled) {
        log::line("[obs] The WebSocket server is off in OBS: Tools › WebSocket Server Settings › Enable.");
    }
    let how = match connection.source() {
        "env" => "KL_OBS_URL / KL_OBS_PASSWORD",
        "manual" => "set on the page",
        _ => "read from OBS's settings",
    };
    log::line(format!("[obs] Looking for OBS at {} ({how})", obs.url()));
    obs.start();
    watcher.start();
    log::line(format!("Control page: {}/", server.url));

    Ok(App { url: server.url.clone(), data_dir: opts.data_dir, controller, server })
}
