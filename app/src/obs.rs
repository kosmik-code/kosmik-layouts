//! Minimal obs-websocket v5 client (built into OBS 28+): handshake with optional password, requests,
//! automatic reconnect. Only the few requests this app needs; no event subscriptions. Same behaviour
//! as src/obs.js.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

const OP_HELLO: u64 = 0;
const OP_IDENTIFY: u64 = 1;
const OP_IDENTIFIED: u64 = 2;
const OP_REQUEST: u64 = 6;
const OP_REQUEST_RESPONSE: u64 = 7;
const NOT_RUNNING: &str = "OBS is not running or WebSocket server is off";

fn sha256_b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()))
}

/// obs-websocket auth string: base64(sha256(base64(sha256(password + salt)) + challenge)).
pub fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    sha256_b64(&(sha256_b64(&format!("{password}{salt}")) + challenge))
}

/// Port and password from OBS's own WebSocket settings on this PC.
#[derive(Debug, Clone, PartialEq)]
pub struct ObsSettings {
    pub enabled: bool,
    pub port: u16,
    pub auth_required: bool,
    pub url: String,
    /// Stays in this process: never written by us or sent to the page.
    pub password: String,
}

/// %APPDATA%\obs-studio\plugin_config\obs-websocket\config.json, so nothing has to be typed or copied.
pub fn read_obs_settings(app_data: Option<&Path>) -> Option<ObsSettings> {
    let file = app_data?.join("obs-studio").join("plugin_config").join("obs-websocket").join("config.json");
    let c: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let port = crate::js::js_number(c.get("server_port").unwrap_or(&Value::Null));
    let port = if port.is_finite() && port >= 1.0 && port <= 65535.0 { port as u16 } else { 4455 };
    let auth_required = c["auth_required"] != json!(false);
    let password = match &c["server_password"] {
        Value::Null => String::new(),
        v => crate::js::js_string(v),
    };
    Some(ObsSettings {
        enabled: c["server_enabled"] != json!(false),
        port,
        auth_required,
        url: format!("ws://127.0.0.1:{port}"),
        password: if auth_required { password } else { String::new() },
    })
}

#[derive(Debug, Clone)]
pub struct ObsError {
    pub message: String,
    /// obs-websocket status code (207 = OBS is still starting).
    pub code: Option<i64>,
}

impl ObsError {
    pub fn msg(m: impl Into<String>) -> Self {
        ObsError { message: m.into(), code: None }
    }
}

impl std::fmt::Display for ObsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// state: "connecting" | "connected" | "auth_failed" | "disconnected"
#[derive(Debug, Clone, Serialize)]
pub struct ObsStatus {
    pub state: String,
    pub message: String,
    /// Counts connections, so a reconnect is noticed even when the watcher missed the drop.
    #[serde(skip)]
    pub epoch: u64,
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, ObsError>>>;

struct Inner {
    target: Mutex<(String, String)>,
    status: watch::Sender<ObsStatus>,
    writer: Mutex<Option<(u64, mpsc::UnboundedSender<String>)>>,
    pending: Mutex<Pending>,
    stopped: AtomicBool,
    started: AtomicBool,
    /// Bumped by restart/stop: an older connection attempt may no longer touch the status.
    generation: AtomicU64,
    wake: Notify,
    retry: Duration,
    timeout: Duration,
}

#[derive(Clone)]
pub struct ObsClient(Arc<Inner>);

enum Ended {
    AuthFailed,
    Closed(String),
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ObsClient {
    pub fn new(retry: Duration, timeout: Duration) -> Self {
        let (status, _) = watch::channel(ObsStatus { state: "disconnected".into(), message: String::new(), epoch: 0 });
        ObsClient(Arc::new(Inner {
            target: Mutex::new((String::new(), String::new())),
            status,
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(true),
            started: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            wake: Notify::new(),
            retry,
            timeout,
        }))
    }

    pub fn url(&self) -> String {
        lock(&self.0.target).0.clone()
    }

    /// Sets address and password; true when either changed.
    pub fn set_target(&self, url: &str, password: &str) -> bool {
        let mut t = lock(&self.0.target);
        let changed = t.0 != url || t.1 != password;
        *t = (url.to_string(), password.to_string());
        changed
    }

    pub fn status(&self) -> ObsStatus {
        self.0.status.borrow().clone()
    }

    pub fn status_watch(&self) -> watch::Receiver<ObsStatus> {
        self.0.status.subscribe()
    }

    pub fn connected(&self) -> bool {
        self.0.status.borrow().state == "connected"
    }

    pub fn stopped(&self) -> bool {
        self.0.stopped.load(Ordering::SeqCst)
    }

    fn set_status(&self, state: &str, message: &str) {
        self.0.status.send_if_modified(|s| {
            if s.state == state && s.message == message {
                return false;
            }
            if state == "connected" {
                s.epoch += 1;
            }
            s.state = state.into();
            s.message = message.into();
            true
        });
    }

    fn set_status_for(&self, generation: u64, state: &str, message: &str) {
        if self.0.generation.load(Ordering::SeqCst) == generation {
            self.set_status(state, message);
        }
    }

    fn fail_pending(&self, why: &str) {
        for (_, tx) in lock(&self.0.pending).drain() {
            let _ = tx.send(Err(ObsError::msg(why)));
        }
    }

    pub fn start(&self) {
        self.0.stopped.store(false, Ordering::SeqCst);
        if self.0.started.swap(true, Ordering::SeqCst) {
            self.0.wake.notify_one();
        } else {
            let me = self.clone();
            tokio::spawn(async move { me.supervise().await });
        }
    }

    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.0.writer) = None;
        self.fail_pending("OBS connection closed");
        self.set_status("disconnected", "");
        self.0.wake.notify_one();
    }

    /// Drop the current connection (if any) and connect again now, e.g. after the address or password changed.
    pub fn restart(&self) {
        self.0.stopped.store(false, Ordering::SeqCst);
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.0.writer) = None;
        self.fail_pending("OBS connection closed");
        if self.connected() {
            self.set_status("disconnected", "reconnecting");
        }
        self.set_status("connecting", "");
        if self.0.started.swap(true, Ordering::SeqCst) {
            self.0.wake.notify_one();
        } else {
            let me = self.clone();
            tokio::spawn(async move { me.supervise().await });
        }
    }

    async fn supervise(self) {
        loop {
            if self.stopped() {
                self.0.wake.notified().await;
                continue;
            }
            let generation = self.0.generation.load(Ordering::SeqCst);
            self.set_status_for(generation, "connecting", "");
            let ended = tokio::select! {
                e = self.attempt(generation) => e,
                _ = self.0.wake.notified() => continue,
            };
            {
                let mut w = lock(&self.0.writer);
                if w.as_ref().is_some_and(|(g, _)| *g == generation) {
                    *w = None;
                }
            }
            if self.0.generation.load(Ordering::SeqCst) == generation {
                self.fail_pending("OBS connection closed");
            }
            match ended {
                // 4009 = authentication failed: retrying with the same password only spams OBS's log.
                Ended::AuthFailed => {
                    self.set_status_for(generation, "auth_failed", "OBS rejected the WebSocket password");
                    self.0.wake.notified().await;
                }
                Ended::Closed(reason) => {
                    self.set_status_for(generation, "disconnected", &reason);
                    tokio::select! {
                        _ = tokio::time::sleep(self.0.retry) => {}
                        _ = self.0.wake.notified() => {}
                    }
                }
            }
        }
    }

    async fn attempt(&self, generation: u64) -> Ended {
        let (url, password) = lock(&self.0.target).clone();
        let mut req = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return Ended::Closed(e.to_string()),
        };
        req.headers_mut().insert("Sec-WebSocket-Protocol", tungstenite::http::HeaderValue::from_static("obswebsocket.json"));
        let ws = match tokio::time::timeout(self.0.timeout, tokio_tungstenite::connect_async(req)).await {
            Ok(Ok((ws, _))) => ws,
            _ => return Ended::Closed(NOT_RUNNING.into()),
        };
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        match msg["op"].as_u64() {
                            Some(OP_HELLO) => {
                                let mut d = json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
                                let auth = &msg["d"]["authentication"];
                                if auth.is_object() {
                                    d["authentication"] = json!(auth_string(&password, auth["salt"].as_str().unwrap_or(""), auth["challenge"].as_str().unwrap_or("")));
                                }
                                let out = json!({ "op": OP_IDENTIFY, "d": d }).to_string();
                                if sink.send(Message::Text(out.into())).await.is_err() {
                                    return Ended::Closed(NOT_RUNNING.into());
                                }
                            }
                            Some(OP_IDENTIFIED) => {
                                if self.0.generation.load(Ordering::SeqCst) != generation {
                                    return Ended::Closed(String::new());
                                }
                                *lock(&self.0.writer) = Some((generation, tx.clone()));
                                self.set_status_for(generation, "connected", "");
                            }
                            Some(OP_REQUEST_RESPONSE) => self.resolve(&msg["d"]),
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code));
                        if code == Some(4009) {
                            return Ended::AuthFailed;
                        }
                        let reason = frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| NOT_RUNNING.into());
                        return Ended::Closed(reason);
                    }
                    Some(Ok(_)) => {}
                    None | Some(Err(_)) => return Ended::Closed(NOT_RUNNING.into()),
                },
                Some(out) = rx.recv() => {
                    if sink.send(Message::Text(out.into())).await.is_err() {
                        return Ended::Closed(NOT_RUNNING.into());
                    }
                }
            }
        }
    }

    fn resolve(&self, d: &Value) {
        let Some(id) = d["requestId"].as_str() else { return };
        let Some(tx) = lock(&self.0.pending).remove(id) else { return };
        let r = if d["requestStatus"]["result"] == json!(true) {
            Ok(match &d["responseData"] {
                Value::Null => json!({}),
                v => v.clone(),
            })
        } else {
            let st = &d["requestStatus"];
            let what = match st["comment"].as_str().filter(|c| !c.is_empty()) {
                Some(c) => c.to_string(),
                None => format!("code {}", crate::js::js_string(&st["code"])),
            };
            Err(ObsError { message: format!("{} failed: {what}", d["requestType"].as_str().unwrap_or("request")), code: st["code"].as_i64() })
        };
        let _ = tx.send(r);
    }

    /// Send one request; resolves with responseData.
    pub async fn call(&self, request_type: &str, request_data: Value) -> Result<Value, ObsError> {
        let writer = if self.connected() { lock(&self.0.writer).as_ref().map(|(_, w)| w.clone()) } else { None };
        let Some(writer) = writer else { return Err(ObsError::msg("OBS is not connected")) };
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        lock(&self.0.pending).insert(id.clone(), tx);
        let msg = json!({ "op": OP_REQUEST, "d": { "requestType": request_type, "requestId": id, "requestData": request_data } });
        if writer.send(msg.to_string()).is_err() {
            lock(&self.0.pending).remove(&id);
            return Err(ObsError::msg("OBS is not connected"));
        }
        match tokio::time::timeout(self.0.timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(ObsError::msg("OBS connection closed")),
            Err(_) => {
                lock(&self.0.pending).remove(&id);
                Err(ObsError::msg(format!("{request_type} timed out")))
            }
        }
    }

    /// Waits until the status matches (tests and the connection settings).
    pub async fn wait_for(&self, ms: u64, pred: impl Fn(&ObsStatus) -> bool) -> bool {
        let mut rx = self.status_watch();
        tokio::time::timeout(Duration::from_millis(ms), async {
            loop {
                if pred(&rx.borrow_and_update()) {
                    return;
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
        .await
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_obs::{FakeObs, item};

    fn client(url: &str, password: &str, retry_ms: u64) -> ObsClient {
        let obs = ObsClient::new(Duration::from_millis(retry_ms), Duration::from_secs(5));
        obs.set_target(url, password);
        obs
    }

    #[test]
    fn auth_string_follows_the_four_documented_steps() {
        let (password, salt, challenge) = ("supersecretpassword", "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=", "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=");
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let secret = b64(&Sha256::digest(format!("{password}{salt}").as_bytes()));
        let expected = b64(&Sha256::digest(format!("{secret}{challenge}").as_bytes()));
        assert_eq!(auth_string(password, salt, challenge), expected);
        assert_ne!(auth_string("other", salt, challenge), expected);
    }

    #[tokio::test]
    async fn connects_with_the_right_password_and_answers_requests() {
        let fake = FakeObs::start("pw", vec![("Live", vec![item(1, "Camera", 10.0, 20.0)])]).await;
        let obs = client(&fake.url, "pw", 50);
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        let list = obs.call("GetSceneItemList", json!({ "sceneName": "Live" })).await.unwrap();
        assert_eq!(list["sceneItems"][0]["sourceName"], "Camera");
        let err = obs.call("GetSceneItemList", json!({ "sceneName": "Nope" })).await.unwrap_err();
        assert!(err.message.contains("GetSceneItemList failed"), "{}", err.message);
        obs.stop();
    }

    #[tokio::test]
    async fn wrong_password_stops_at_auth_failed_without_retrying() {
        let fake = FakeObs::start("pw", vec![("Live", vec![])]).await;
        let obs = client(&fake.url, "nope", 20);
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "auth_failed").await);
        let tries = fake.connections();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(obs.status().state, "auth_failed");
        assert_eq!(fake.connections(), tries, "no retries with the same password");
        obs.stop();
    }

    #[tokio::test]
    async fn reconnects_after_obs_drops_the_connection() {
        let fake = FakeObs::start("", vec![("Live", vec![])]).await;
        let obs = client(&fake.url, "", 30);
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        let first = obs.status().epoch;
        fake.drop_clients();
        assert!(obs.wait_for(3000, |s| s.state == "connected" && s.epoch > first).await);
        obs.stop();
    }

    #[test]
    fn reads_port_and_password_from_obs_settings() {
        let app_data = tempfile::tempdir().unwrap();
        let dir = app_data.path().join("obs-studio/plugin_config/obs-websocket");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"server_enabled":true,"server_port":4460,"auth_required":true,"server_password":"s3cret"}"#).unwrap();
        assert_eq!(
            read_obs_settings(Some(app_data.path())),
            Some(ObsSettings { enabled: true, port: 4460, auth_required: true, url: "ws://127.0.0.1:4460".into(), password: "s3cret".into() })
        );
        assert_eq!(read_obs_settings(Some(&app_data.path().join("missing"))), None);
    }
}
