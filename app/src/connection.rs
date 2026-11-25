//! Where OBS is and how to log in. Three sources, strongest first:
//!   env     KL_OBS_URL / KL_OBS_PASSWORD (fixed for this run; the page shows it but cannot change it)
//!   manual  host, port and password typed on the page (OBS on another PC, a changed port...)
//!   auto    read from OBS's own WebSocket settings on this PC (the default: nothing to type)
//! Manual values live in obs-connection.json. The password never leaves this process: the page only
//! learns whether one is saved. Same behaviour as src/connection.js.

use crate::js::{js_number, js_string, utf16_len};
use crate::obs::{ObsClient, ObsSettings};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 4455;
const MAX_PASSWORD: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct Manual {
    pub mode: String,
    pub host: String,
    pub port: u16,
    pub password: String,
}

impl Default for Manual {
    fn default() -> Self {
        Manual { mode: "auto".into(), host: "127.0.0.1".into(), port: DEFAULT_PORT, password: String::new() }
    }
}

impl Manual {
    fn to_json(&self) -> Value {
        json!({ "mode": self.mode, "host": self.host, "port": self.port, "password": self.password })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

fn field(f: &str, m: &str) -> FieldError {
    FieldError { field: f.into(), message: m.into() }
}

/// "ws://192.168.1.20:4455" or "192.168.1.20:4455" typed into the host field -> (host, port).
pub fn split_address(input: &str) -> (String, Option<u16>) {
    let s = input.trim();
    if s.is_empty() {
        return (String::new(), None);
    }
    let has_scheme = s.find("://").is_some_and(|i| i > 0 && s[..i].chars().all(|c| c.is_ascii_alphabetic()));
    let full = if has_scheme { s.to_string() } else { format!("ws://{s}") };
    match url::Url::parse(&full) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_string();
            (host, u.port())
        }
        Err(_) => (s.to_string(), None),
    }
}

fn is_label(l: &str) -> bool {
    let b = l.as_bytes();
    !b.is_empty() && b[0].is_ascii_alphanumeric() && b[b.len() - 1].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
}

pub fn is_host(h: &str) -> bool {
    let hostname = (1..=253).contains(&h.len()) && h.split('.').all(is_label);
    let ipv6 = h.contains(':') && (2..=45).contains(&h.len()) && h.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    hostname || ipv6
}

pub fn ws_url(host: &str, port: u16) -> String {
    if host.contains(':') { format!("ws://[{host}]:{port}") } else { format!("ws://{host}:{port}") }
}

/// Errors (field by field, so the page can show them next to the input) or the cleaned settings.
pub fn validate_manual(input: &Value, previous: &Manual) -> (Vec<FieldError>, Manual) {
    let mut errors = Vec::new();
    let mut out = previous.clone();
    if let Some(mode) = input.get("mode") {
        match mode.as_str() {
            Some(m @ ("auto" | "manual")) => out.mode = m.into(),
            _ => errors.push(field("mode", "must be auto or manual")),
        }
    }
    if let Some(h) = input.get("host") {
        let (host, port) = split_address(&js_string(h));
        if is_host(&host) {
            out.host = host;
        } else {
            errors.push(field("host", "must be an address like 127.0.0.1 or my-pc.local"));
        }
        if let (Some(p), None) = (port, input.get("port")) {
            out.port = p;
        }
    }
    if let Some(p) = input.get("port") {
        let port = js_number(p);
        if port.fract() == 0.0 && (1.0..=65535.0).contains(&port) {
            out.port = port as u16;
        } else {
            errors.push(field("port", "must be a number from 1 to 65535"));
        }
    }
    if input.get("clearPassword") == Some(&json!(true)) {
        out.password = String::new();
    } else if let Some(pw) = input.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()) {
        if utf16_len(pw) > MAX_PASSWORD {
            errors.push(field("password", &format!("at most {MAX_PASSWORD} characters")));
        } else {
            out.password = pw.into();
        }
    }
    (errors, out)
}

pub type ReadSettings = Arc<dyn Fn() -> Option<ObsSettings> + Send + Sync>;

struct State {
    manual: Manual,
    /// OBS's own settings on this PC, None when not found.
    detected: Option<ObsSettings>,
}

pub struct Connection {
    obs: ObsClient,
    file: PathBuf,
    env_url: String,
    env_password: Option<String>,
    read_settings: ReadSettings,
    recheck: Duration,
    st: Mutex<State>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Connection {
    /// env_url / env_password: KL_OBS_URL / KL_OBS_PASSWORD (None when unset).
    pub fn load(obs: ObsClient, data_dir: &Path, env_url: Option<String>, env_password: Option<String>, read_settings: ReadSettings) -> Arc<Connection> {
        let file = data_dir.join("obs-connection.json");
        let mut manual = Manual::default();
        // First run or unreadable: automatic.
        if let Some(saved) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            let (errors, clean) = validate_manual(&saved, &Manual::default());
            if errors.is_empty() {
                manual = clean;
            }
        }
        let detected = read_settings();
        let c = Arc::new(Connection {
            obs,
            file,
            env_url: env_url.unwrap_or_default(),
            env_password,
            read_settings,
            recheck: Duration::from_secs(4),
            st: Mutex::new(State { manual, detected }),
            watcher: Mutex::new(None),
        });
        c.apply(false);
        c
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn source(&self) -> &'static str {
        if !self.env_url.is_empty() || self.env_password.is_some() {
            "env"
        } else if self.lock().manual.mode == "manual" {
            "manual"
        } else {
            "auto"
        }
    }

    pub fn detected(&self) -> Option<ObsSettings> {
        self.lock().detected.clone()
    }

    /// What the OBS client should use right now.
    pub fn target(&self) -> (String, String) {
        let source = self.source();
        let st = self.lock();
        let d = st.detected.as_ref();
        let auto_url = d.map(|d| d.url.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| ws_url("127.0.0.1", DEFAULT_PORT));
        let detected_pw = d.map(|d| d.password.clone()).unwrap_or_default();
        match source {
            "env" => (if self.env_url.is_empty() { auto_url } else { self.env_url.clone() }, self.env_password.clone().unwrap_or(detected_pw)),
            "manual" => (ws_url(&st.manual.host, st.manual.port), st.manual.password.clone()),
            _ => (auto_url, detected_pw),
        }
    }

    /// Point the client at target(); reconnects when something changed (or when forced).
    pub fn apply(&self, force: bool) -> bool {
        let (url, password) = self.target();
        let changed = self.obs.set_target(&url, &password);
        if (changed || force) && !self.obs.stopped() {
            self.obs.restart();
        }
        changed
    }

    /// Save the page's choices and reconnect. Returns once OBS answered (or after wait),
    /// so the page can say right away whether the new address and password work.
    pub async fn save(&self, input: &Value, wait: Duration) -> Result<(), Vec<FieldError>> {
        if self.source() == "env" {
            return Err(vec![field("mode", "set by KL_OBS_URL / KL_OBS_PASSWORD")]);
        }
        let current = self.lock().manual.clone();
        let (errors, next) = validate_manual(input, &current);
        if !errors.is_empty() {
            return Err(errors);
        }
        crate::store::write_json_atomic(&self.file, &next.to_json()).map_err(|e| vec![field("mode", &e.to_string())])?;
        let auto = next.mode == "auto";
        self.lock().manual = next;
        if auto {
            let now = (self.read_settings)();
            self.lock().detected = now;
        }
        self.apply(true);
        self.settled(wait).await;
        Ok(())
    }

    /// Waits until the client is connected or has clearly failed (wrong password, nobody listening).
    pub async fn settled(&self, wait: Duration) {
        self.obs
            .wait_for(wait.as_millis() as u64, |s| s.state == "connected" || s.state == "auth_failed" || (s.state == "disconnected" && s.message != "reconnecting"))
            .await;
    }

    /// While not connected in automatic mode, re-read OBS's settings: turning the server on for the first
    /// time creates a new password, and people change ports. That should just work without a restart.
    pub fn watch(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let every = self.recheck;
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = me.upgrade() else { return };
                if c.obs.connected() || c.source() == "manual" {
                    continue;
                }
                let now = (c.read_settings)();
                let found = now.is_some();
                c.lock().detected = now;
                // apply() reconnects when the port or password changed; that also un-sticks an auth
                // failure, which does not retry on its own.
                if c.source() == "auto" && found {
                    c.apply(false);
                }
            }
        });
        *self.watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }

    pub fn stop(&self) {
        if let Some(t) = self.watcher.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }

    /// Public view for the page. Never includes a password.
    pub fn view(&self) -> Value {
        let source = self.source();
        let st = self.lock();
        json!({
            "source": source,
            "url": self.obs.url(),
            "mode": st.manual.mode,
            "host": st.manual.host,
            "port": st.manual.port,
            "hasPassword": !st.manual.password.is_empty(),
            "env": if source == "env" { json!({ "url": !self.env_url.is_empty(), "password": self.env_password.is_some() }) } else { Value::Null },
            "detected": st.detected.as_ref().map(|d| json!({ "enabled": d.enabled, "port": d.port, "authRequired": d.auth_required })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_obs::{FakeObs, item};

    fn detected(port: u16, password: &str, url: &str) -> ReadSettings {
        let s = ObsSettings { enabled: true, port, auth_required: !password.is_empty(), url: url.into(), password: password.into() };
        Arc::new(move || Some(s.clone()))
    }

    fn client() -> ObsClient {
        ObsClient::new(Duration::from_millis(30), Duration::from_secs(5))
    }

    #[test]
    fn manual_settings_addresses_ports_and_passwords_are_checked() {
        assert_eq!(split_address("ws://192.168.1.20:4460/"), ("192.168.1.20".into(), Some(4460)));
        assert_eq!(split_address("obs-pc.local"), ("obs-pc.local".into(), None));
        assert_eq!(ws_url("::1", 4455), "ws://[::1]:4455");
        assert_eq!(split_address("[::1]:4460"), ("::1".into(), Some(4460)));

        let (errors, out) = validate_manual(&json!({ "mode": "manual", "host": "192.168.1.20:4460" }), &Manual::default());
        assert!(errors.is_empty());
        assert_eq!((out.host.as_str(), out.port), ("192.168.1.20", 4460), "a port typed with the address is used");
        assert_eq!(validate_manual(&json!({ "host": "bad host/x" }), &Manual::default()).0[0].field, "host");
        assert_eq!(validate_manual(&json!({ "port": 70000 }), &Manual::default()).0[0].field, "port");
        assert_eq!(validate_manual(&json!({ "mode": "remote" }), &Manual::default()).0[0].field, "mode");

        // An empty password field keeps the saved one; clearing it is explicit.
        let old = Manual { mode: "manual".into(), host: "h".into(), port: 1, password: "old".into() };
        assert_eq!(validate_manual(&json!({ "password": "" }), &old).1.password, "old");
        assert_eq!(validate_manual(&json!({ "clearPassword": true }), &old).1.password, "");
    }

    #[tokio::test]
    async fn automatic_follows_obs_manual_wins_env_wins_over_both() {
        let dir = tempfile::tempdir().unwrap();
        let read = detected(4460, "fromobs", "ws://127.0.0.1:4460");
        let obs = client();
        let c = Connection::load(obs.clone(), dir.path(), None, None, read.clone());
        assert_eq!(c.source(), "auto");
        assert_eq!(c.target(), ("ws://127.0.0.1:4460".into(), "fromobs".into()));
        assert_eq!(obs.url(), "ws://127.0.0.1:4460");

        c.save(&json!({ "mode": "manual", "host": "10.0.0.5", "port": 4999, "password": "typed" }), Duration::ZERO).await.unwrap();
        assert_eq!(c.source(), "manual");
        assert_eq!(c.target(), ("ws://10.0.0.5:4999".into(), "typed".into()));
        obs.stop();
        let view = c.view().to_string();
        assert!(view.contains("\"hasPassword\":true"));
        assert!(!view.contains("typed") && !view.contains("fromobs"), "the page never sees the password");
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("obs-connection.json")).unwrap()).unwrap();
        assert_eq!(on_disk["password"], "typed");

        // Saved choices survive a restart.
        let c2 = Connection::load(client(), dir.path(), None, None, read.clone());
        assert_eq!((c2.source(), c2.target()), ("manual", ("ws://10.0.0.5:4999".into(), "typed".into())));

        let c3 = Connection::load(client(), dir.path(), Some("ws://1.2.3.4:1".into()), None, read);
        assert_eq!((c3.source(), c3.target()), ("env", ("ws://1.2.3.4:1".into(), "fromobs".into())));
        assert!(c3.save(&json!({ "mode": "auto" }), Duration::ZERO).await.is_err(), "env settings can't be changed from the page");
    }

    #[tokio::test]
    async fn saving_a_new_password_reconnects_and_reports_whether_obs_accepted_it() {
        let fake = FakeObs::start("right", vec![("Live", vec![item(1, "Camera", 0.0, 0.0)])]).await;
        let port: u16 = fake.url.rsplit(':').next().unwrap().parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let obs = client();
        let c = Connection::load(obs.clone(), dir.path(), None, None, Arc::new(|| None));
        c.save(&json!({ "mode": "manual", "host": "127.0.0.1", "port": port, "password": "wrong" }), Duration::ZERO).await.unwrap();
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "auth_failed").await);

        c.save(&json!({ "password": "right" }), Duration::from_secs(3)).await.unwrap();
        assert_eq!(obs.status().state, "connected");
        assert_eq!(c.view()["port"], port);
        obs.stop();
    }
}
