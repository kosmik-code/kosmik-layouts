//! Local HTTP server for the control page. Loopback only; every request needs a local Host header and
//! every change a same-origin Origin header, so other websites can neither read nor drive it.
//! Same routes and guards as src/server.js.

use crate::connection::Connection;
use crate::controller::Controller;
use crate::js::{js_number, js_string, num, truthy, utf16_take};
use crate::store::{MAX_PROFILES, UpdateError, obj_mut, profiles_mut, slugify};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

const MAX_JSON: usize = 256 * 1024;

/// The control page: built into the program, or read from a folder while developing.
pub enum PublicFiles {
    Embedded(&'static include_dir::Dir<'static>),
    Disk(PathBuf),
}

impl PublicFiles {
    fn get(&self, name: &str) -> Option<Cow<'static, [u8]>> {
        match self {
            PublicFiles::Embedded(dir) => dir.get_file(name).map(|f| Cow::Borrowed(f.contents())),
            PublicFiles::Disk(root) => {
                let path = root.join(name);
                path.is_file().then(|| std::fs::read(path).ok().map(Cow::Owned)).flatten()
            }
        }
    }
}

/// index.html, app.js... : one level, lowercase, known types only.
fn is_page_file(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && ["html", "js", "css", "svg"].contains(&ext)
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "image/svg+xml",
    }
}

struct HttpError {
    status: u16,
    body: Value,
}

impl HttpError {
    fn new(status: u16, msg: &str) -> Self {
        HttpError { status, body: json!({ "error": msg }) }
    }
    fn with(status: u16, msg: &str, extra: Value) -> Self {
        let mut body = json!({ "error": msg });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        HttpError { status, body }
    }
}

type Res = Result<Response, HttpError>;

fn reply(status: u16, content_type: Option<&str>, body: impl Into<Body>) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = res.headers_mut();
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(ct) {
            h.insert(header::CONTENT_TYPE, v);
        }
        if ct.starts_with("text/html") {
            h.insert("Content-Security-Policy", HeaderValue::from_static("frame-ancestors 'self'"));
        }
    }
    res
}

fn json_reply(status: u16, body: &Value) -> Response {
    reply(status, Some("application/json; charset=utf-8"), body.to_string())
}

fn header_str(h: &HeaderMap, name: header::HeaderName) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_lowercase)
}

/// Reads up to `limit` bytes. A bigger body is still read to the end (and dropped, up to a cap)
/// so the browser gets the 413 answer instead of a reset connection.
async fn read_body(req: Request, limit: usize) -> Result<Bytes, HttpError> {
    use futures_util::StreamExt;
    const DRAIN_CAP: usize = 16 * 1024 * 1024;
    let mut stream = req.into_body().into_data_stream();
    let mut buf = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| HttpError::new(400, "invalid JSON"))?;
        total += chunk.len();
        if total <= limit {
            buf.extend_from_slice(&chunk);
        } else if total > DRAIN_CAP {
            break;
        }
    }
    if total > limit {
        return Err(HttpError::new(413, "payload too large"));
    }
    Ok(Bytes::from(buf))
}

async fn read_json(req: Request) -> Result<Value, HttpError> {
    if !header_str(req.headers(), header::CONTENT_TYPE).is_some_and(|ct| ct.starts_with("application/json")) {
        return Err(HttpError::new(415, "JSON body required"));
    }
    let body = read_body(req, MAX_JSON).await?;
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&body).map_err(|_| HttpError::new(400, "invalid JSON"))
}

/// "EldenRing" -> "eldenring.exe": trimmed, lower-case, unique, with .exe.
fn clean_processes(list: &Value) -> Value {
    let mut out: Vec<String> = Vec::new();
    for x in list.as_array().into_iter().flatten() {
        let s = js_string(x).trim().to_lowercase();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    Value::Array(out.into_iter().map(|x| Value::String(if x.ends_with(".exe") { x } else { format!("{x}.exe") })).collect())
}

pub type Installer = Arc<dyn Fn() + Send + Sync>;
pub type ListApps = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;

struct AppState {
    ctl: Arc<Controller>,
    connection: Option<Arc<Connection>>,
    list_apps: ListApps,
    public: PublicFiles,
    update: Mutex<Value>,
    installer: Mutex<Option<Installer>>,
    hosts: HashSet<String>,
    origins: HashSet<String>,
    hotkeys: Arc<crate::hotkeys::Hotkeys>,
}

impl AppState {
    /// The controller's state plus the app's own version and update status.
    fn state(&self) -> Value {
        let mut s = self.ctl.state();
        s["version"] = json!(crate::VERSION);
        s["update"] = self.update.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s["hotkeys"] = self.hotkeys.view();
        s
    }

    fn ok(&self, status: u16) -> Res {
        Ok(json_reply(status, &self.state()))
    }

    /// Store change, then a sync (not awaited). before_sync runs between the two.
    fn save<R>(&self, f: impl FnOnce(&mut Value) -> Result<R, (u16, String)>, before_sync: impl FnOnce(&R)) -> Result<R, HttpError> {
        let r = self.ctl.store.update(f).map_err(|e| match e {
            UpdateError::Invalid(errors) => HttpError::with(400, "invalid data", json!({ "errors": errors })),
            UpdateError::Rejected(status, msg) => HttpError::new(status, &msg),
            UpdateError::Io(m) => {
                crate::log::line(format!("[server] {m}"));
                HttpError::new(500, "internal error")
            }
        })?;
        before_sync(&r);
        let ctl = self.ctl.clone();
        tokio::spawn(async move { ctl.sync("profiles changed").await });
        Ok(r)
    }

    fn need_obs(&self) -> Result<(), HttpError> {
        if self.ctl.obs.connected() { Ok(()) } else { Err(HttpError::new(409, "OBS is not connected")) }
    }
}

fn bad_gateway(e: crate::obs::ObsError) -> HttpError {
    HttpError::new(502, &e.message)
}

/// /api/profiles/<id>[/capture|/duplicate] -> (id, action)
fn profile_route(path: &str) -> Option<(String, &str)> {
    let rest = path.strip_prefix("/api/profiles/")?;
    let (id, action) = match rest.split_once('/') {
        Some((id, a @ ("capture" | "duplicate"))) => (id, a),
        Some(_) => return None,
        None => (rest, ""),
    };
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')).then(|| (id.to_string(), action))
}

async fn api(st: Arc<AppState>, req: Request) -> Res {
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let ctl = st.ctl.clone();
    let pm = profile_route(&path);

    match (method.as_str(), path.as_str()) {
        ("GET", "/api/state") => return st.ok(200),
        ("GET", "/api/obs") => {
            let c = st.connection.as_ref().ok_or_else(|| HttpError::new(404, "not found"))?;
            return Ok(json_reply(200, &c.view()));
        }
        ("PUT", "/api/obs") => {
            let c = st.connection.clone().ok_or_else(|| HttpError::new(404, "not found"))?;
            let body = read_json(req).await?;
            if let Err(errors) = c.save(&body, Duration::from_secs(4)).await {
                let list: Vec<String> = errors.iter().map(|e| format!("{}: {}", e.field, e.message)).collect();
                return Err(HttpError::with(400, "invalid connection settings", json!({ "errors": list, "fields": errors })));
            }
            return st.ok(200);
        }
        ("POST", "/api/obs/reconnect") => {
            let c = st.connection.clone().ok_or_else(|| HttpError::new(404, "not found"))?;
            c.apply(true);
            c.settled(Duration::from_secs(4)).await;
            return st.ok(200);
        }
        ("GET", "/api/screenshot") => {
            st.need_obs()?;
            let w = form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "w").map(|(_, v)| v.into_owned());
            let shot = ctl.screenshot(w.as_deref()).await.map_err(bad_gateway)?;
            return Ok(json_reply(200, &shot));
        }
        ("GET", "/api/apps") => {
            let list = st.list_apps.clone();
            let apps = tokio::task::spawn_blocking(move || list()).await.unwrap_or_default();
            return Ok(json_reply(200, &Value::Array(apps)));
        }
        ("PUT", "/api/settings") => {
            let b = read_json(req).await?;
            st.save(
                |d| {
                    if let Some(v) = b.get("scene") {
                        d["scene"] = json!(js_string(v));
                    }
                    if let Some(v) = b.get("animateMs") {
                        d["animateMs"] = num(js_number(v));
                    }
                    if let Some(v) = b.get("defaultProfile") {
                        d["defaultProfile"] = json!(js_string(v));
                    }
                    Ok(())
                },
                |_| {},
            )?;
            ctl.forget_applied(); // scene or default may have changed: re-apply
            let c = ctl.clone();
            tokio::spawn(async move { c.sync("settings changed").await });
            return st.ok(200);
        }
        ("POST", "/api/lock") => {
            let b = read_json(req).await?;
            let id = if truthy(&b["id"]) { Some(js_string(&b["id"])) } else { None };
            ctl.set_lock(id.as_deref()).await;
            return st.ok(200);
        }
        ("POST", "/api/profiles") => {
            // New profile = the current OBS layout, captured now.
            let b = read_json(req).await?;
            st.need_obs()?;
            let (scene, items, canvas) = ctl.capture().await.map_err(bad_gateway)?;
            let n = items.len();
            let raw_name = if b["name"].is_null() { String::new() } else { js_string(&b["name"]) };
            let c2 = ctl.clone();
            st.save(
                |d| {
                    let taken: Vec<String> = d["profiles"].as_array().into_iter().flatten().filter_map(|p| p["id"].as_str().map(String::from)).collect();
                    let id = slugify(&raw_name, &taken);
                    let mut p = json!({ "id": id, "name": raw_name.trim(), "processes": clean_processes(&b["processes"]), "items": items });
                    if let Some(c) = canvas {
                        p["canvas"] = c;
                    }
                    profiles_mut(d).push(p);
                    if !truthy(&d["defaultProfile"]) && truthy(&b["isDefault"]) {
                        d["defaultProfile"] = json!(id);
                    }
                    Ok(id)
                },
                // OBS already shows exactly this layout: no need to re-apply it.
                |id| c2.set_active(Some(id.clone())),
            )?;
            ctl.note("created", json!({ "name": b["name"], "scene": scene, "n": n }), &format!("Created {raw_name} from \"{scene}\" ({n} sources)"));
            return st.ok(201);
        }
        _ => {}
    }

    let Some((id, action)) = pm else { return Err(HttpError::new(404, "not found")) };
    match (method.as_str(), action) {
        ("POST", "duplicate") => {
            // A copy without games, so it never competes with the original; the user adds the game later.
            let src = ctl.profile(&id).ok_or_else(|| HttpError::new(404, "unknown profile"))?;
            let b = read_json(req).await.unwrap_or_else(|_| json!({}));
            let src_name = src["name"].as_str().unwrap_or("").to_string();
            let wanted = if truthy(&b["name"]) { js_string(&b["name"]) } else { format!("{src_name} (2)") };
            let name = utf16_take(wanted.trim(), 60);
            st.save(
                |d| {
                    let profiles = profiles_mut(d);
                    if profiles.len() >= MAX_PROFILES {
                        return Err((400, format!("at most {MAX_PROFILES} layouts")));
                    }
                    let taken: Vec<String> = profiles.iter().filter_map(|p| p["id"].as_str().map(String::from)).collect();
                    let mut copy = src.clone();
                    let o = obj_mut(&mut copy);
                    o.insert("id".into(), json!(slugify(&name, &taken)));
                    o.insert("name".into(), json!(name));
                    o.insert("processes".into(), json!([]));
                    let at = profiles.iter().position(|p| p["id"].as_str() == Some(id.as_str())).map_or(0, |i| i + 1);
                    profiles.insert(at, copy);
                    Ok(())
                },
                |_| {},
            )?;
            ctl.note("duplicated", json!({ "name": src_name, "copy": name }), &format!("Duplicated {src_name} as {name}"));
            st.ok(201)
        }
        ("POST", "capture") => {
            if ctl.profile(&id).is_none() {
                return Err(HttpError::new(404, "unknown profile"));
            }
            st.need_obs()?;
            let (scene, mut items, canvas) = ctl.capture().await.map_err(bad_gateway)?;
            let c2 = ctl.clone();
            let id2 = id.clone();
            st.save(
                |d| {
                    let p = profiles_mut(d).iter_mut().find(|p| p["id"].as_str() == Some(id.as_str())).ok_or((404, "unknown profile".to_string()))?;
                    // Keep the include/exclude choices the user already made for known sources.
                    for (name, it) in items.iter_mut() {
                        if p["items"].get(name).and_then(|x| x.get("include")) == Some(&json!(false)) {
                            obj_mut(it).insert("include".into(), json!(false));
                        }
                    }
                    p["items"] = Value::Object(items);
                    if let Some(c) = canvas {
                        p["canvas"] = c;
                    }
                    Ok(())
                },
                |_| c2.set_active(Some(id2)),
            )?;
            let name = ctl.profile(&id).and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default();
            ctl.note("recaptured", json!({ "name": name, "scene": scene }), &format!("Updated {name} from \"{scene}\""));
            st.ok(200)
        }
        ("PUT", "") => {
            let b = read_json(req).await?;
            st.save(
                |d| {
                    let p = profiles_mut(d).iter_mut().find(|p| p["id"].as_str() == Some(id.as_str())).ok_or((404, "unknown profile".to_string()))?;
                    if let Some(n) = b.get("name") {
                        p["name"] = json!(js_string(n).trim());
                    }
                    if let Some(list) = b.get("processes") {
                        p["processes"] = clean_processes(list);
                    }
                    let include: Vec<(String, bool)> = match &b["include"] {
                        Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), truthy(v))).collect(),
                        Value::Array(a) => a.iter().enumerate().map(|(i, v)| (i.to_string(), truthy(v))).collect(),
                        _ => vec![],
                    };
                    for (name, on) in include {
                        if let Some(it) = p["items"].get_mut(&name).filter(|it| truthy(it)) {
                            obj_mut(it).insert("include".into(), json!(on));
                        }
                    }
                    Ok(())
                },
                |_| {},
            )?;
            st.ok(200)
        }
        ("DELETE", "") => {
            st.save(
                |d| {
                    profiles_mut(d).retain(|p| p["id"].as_str() != Some(id.as_str()));
                    if d["defaultProfile"].as_str() == Some(id.as_str()) {
                        d["defaultProfile"] = json!("");
                    }
                    Ok(())
                },
                |_| {},
            )?;
            ctl.forget_profile(&id);
            st.ok(200)
        }
        ("POST", "") => {
            // Apply once without locking (preview a profile).
            if ctl.profile(&id).is_none() {
                return Err(HttpError::new(404, "unknown profile"));
            }
            st.need_obs()?;
            ctl.queued(ctl.apply_now(&id, "applied by hand")).await.map_err(bad_gateway)?;
            st.ok(200)
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

async fn route(st: Arc<AppState>, req: Request) -> Res {
    let host = header_str(req.headers(), header::HOST).unwrap_or_default();
    if !st.hosts.contains(&host) {
        return Err(HttpError::new(403, "forbidden host"));
    }
    let read_only = req.method() == Method::GET || req.method() == Method::HEAD;
    let origin = header_str(req.headers(), header::ORIGIN).unwrap_or_default();
    if !read_only && !st.origins.contains(&origin) {
        return Err(HttpError::new(403, "forbidden origin"));
    }
    let path = req.uri().path().to_string();
    if path == "/api/update/install" && req.method() == Method::POST {
        let ready = st.update.lock().unwrap_or_else(|e| e.into_inner())["install"] == json!(true);
        let installer = st.installer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        return match installer {
            Some(install) if ready => {
                install();
                Ok(reply(202, None, Body::empty()))
            }
            _ => Err(HttpError::new(409, "no update to install")),
        };
    }
    if path == "/api/hotkeys" && req.method() == Method::PUT {
        let b = read_json(req).await?;
        return match st.hotkeys.set(b["id"].as_str().unwrap_or(""), b["key"].as_str().unwrap_or("")) {
            Ok(_) => st.ok(200),
            Err("io") => Err(HttpError::new(500, "internal error")),
            Err(code) => Err(HttpError::new(400, code)),
        };
    }
    // While the page waits for a new combination, the hotkeys are let go of, so even the current one
    // reaches the page.
    if path == "/api/hotkeys/pause" && req.method() == Method::POST {
        let b = read_json(req).await?;
        let Some(on) = b["paused"].as_bool() else { return Err(HttpError::new(400, "paused must be true or false")) };
        st.hotkeys.pause(on);
        return Ok(json_reply(200, &json!({ "paused": on })));
    }
    if path.starts_with("/api/") {
        return api(st, req).await;
    }
    if req.method() != Method::GET {
        return Err(HttpError::new(405, "method not allowed"));
    }
    let name = if path == "/" { "index.html" } else { &path[1..] };
    if !is_page_file(name) {
        return Err(HttpError::new(404, "not found"));
    }
    match st.public.get(name) {
        Some(body) => Ok(reply(
            200,
            Some(content_type(name)),
            match body {
                Cow::Borrowed(b) => Body::from(Bytes::from_static(b)),
                Cow::Owned(v) => Body::from(v),
            },
        )),
        None => Err(HttpError::new(404, "not found")),
    }
}

async fn handle(State(st): State<Arc<AppState>>, req: Request) -> Response {
    match route(st, req).await {
        Ok(r) => r,
        Err(e) => json_reply(e.status, &e.body),
    }
}

pub struct ServerOptions {
    pub controller: Arc<Controller>,
    pub connection: Option<Arc<Connection>>,
    pub list_apps: ListApps,
    pub public: PublicFiles,
    pub hotkeys: Arc<crate::hotkeys::Hotkeys>,
}

pub struct ServerHandle {
    pub url: String,
    pub port: u16,
    state: Arc<AppState>,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub fn set_update(&self, next: Value) {
        *self.state.update.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }

    pub fn set_installer(&self, install: Installer) {
        *self.state.installer.lock().unwrap_or_else(|e| e.into_inner()) = Some(install);
    }

    pub fn close(&self) {
        self.task.abort();
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// Serves on an already bound loopback listener (binding first lets the caller report a busy port).
pub async fn start_server(listener: TcpListener, opts: ServerOptions) -> std::io::Result<ServerHandle> {
    let port = listener.local_addr()?.port();
    let hosts: HashSet<String> = [format!("127.0.0.1:{port}"), format!("localhost:{port}")].into();
    let origins = hosts.iter().map(|h| format!("http://{h}")).collect();
    let state = Arc::new(AppState {
        ctl: opts.controller,
        connection: opts.connection,
        list_apps: opts.list_apps,
        public: opts.public,
        update: Mutex::new(Value::Null),
        installer: Mutex::new(None),
        hosts,
        origins,
        hotkeys: opts.hotkeys,
    });
    let app = axum::Router::new().fallback(handle).with_state(state.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(ServerHandle { url: format!("http://127.0.0.1:{port}"), port, state, task })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::tests::until;
    use crate::fake_obs::{FakeObs, Opts, item, item_with};
    use crate::layout::tests::scene;
    use crate::obs::{ObsClient, ObsSettings};
    use crate::store::Store;
    use reqwest::Method as M;

    struct Ctx {
        url: String,
        port: u16,
        srv: ServerHandle,
        http: reqwest::Client,
        _dir: tempfile::TempDir,
    }

    impl Ctx {
        fn call(&self, method: M, p: &str, body: Option<Value>) -> reqwest::RequestBuilder {
            let mut r = self.http.request(method, format!("{}{p}", self.url)).header("Origin", &self.url);
            if let Some(b) = body {
                r = r.header("Content-Type", "application/json").body(b.to_string());
            }
            r
        }
        async fn json(&self, method: M, p: &str, body: Option<Value>) -> (u16, Value) {
            let r = self.call(method, p, body).send().await.unwrap();
            let s = r.status().as_u16();
            (s, r.json().await.unwrap_or(Value::Null))
        }
    }

    async fn serve(ctl: Arc<Controller>, connection: Option<Arc<Connection>>, dir: tempfile::TempDir) -> Ctx {
        let public = PublicFiles::Embedded(&crate::PUBLIC);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let list_apps: ListApps = Arc::new(|| vec![json!({ "exe": "game.exe", "title": "Game" })]);
        let hotkeys = crate::hotkeys::Hotkeys::load(dir.path(), &crate::HOTKEYS);
        let srv = start_server(listener, ServerOptions { controller: ctl, connection, list_apps, public, hotkeys }).await.unwrap();
        Ctx { url: srv.url.clone(), port: srv.port, srv, http: reqwest::Client::new(), _dir: dir }
    }

    async fn raw_status(port: u16, host: &str, path: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out.split(' ').nth(1).unwrap().parse().unwrap()
    }

    #[tokio::test]
    async fn hotkeys_show_in_the_state_and_can_be_changed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let ctl = Controller::new(ObsClient::new(Duration::from_secs(3), Duration::from_secs(5)), store, None);
        let c = serve(ctl, None, dir).await;
        let (_, s) = c.json(M::GET, "/api/state", None).await;
        assert_eq!(s["hotkeys"][0], json!({ "id": "lock", "key": "Ctrl+Alt+Shift+L", "default": "Ctrl+Alt+Shift+L", "error": null }));
        assert_eq!(s["hotkeys"][1]["key"], "Ctrl+Alt+Shift+N");
        let (status, e) = c.json(M::PUT, "/api/hotkeys", Some(json!({ "id": "next", "key": "Ctrl+Alt+Shift+L" }))).await;
        assert_eq!((status, e["error"].clone()), (400, json!("in_use_here")));
        let (status, s) = c.json(M::PUT, "/api/hotkeys", Some(json!({ "id": "next", "key": "ctrl+alt+9" }))).await;
        assert_eq!((status, s["hotkeys"][1]["key"].clone()), (200, json!("Ctrl+Alt+9")));
        let (status, _) = c.json(M::POST, "/api/hotkeys/pause", Some(json!({ "paused": true }))).await;
        assert_eq!(status, 200);
        let foreign = c.http.put(format!("{}/api/hotkeys", c.url)).header("Origin", "https://evil.example").header("Content-Type", "application/json").body(r#"{"id":"lock","key":""}"#);
        assert_eq!(foreign.send().await.unwrap().status(), 403);
    }

    #[tokio::test]
    async fn create_rename_exclude_lock_delete_and_foreign_requests_refused() {
        let fake = FakeObs::start("", scene()).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let obs = ObsClient::new(Duration::from_millis(30), Duration::from_secs(5));
        obs.set_target(&fake.url, "");
        let ctl = Controller::new(obs.clone(), store, None);
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        let c = serve(ctl, None, dir).await;

        let (status, s) = c.json(M::POST, "/api/profiles", Some(json!({ "name": "Elden Ring", "processes": ["EldenRing"], "isDefault": true }))).await;
        assert_eq!(status, 201);
        assert_eq!(s["profiles"][0]["processes"], json!(["eldenring.exe"]));
        assert_eq!(s["profiles"][0]["id"], "elden-ring");
        assert_eq!(s["defaultProfile"], "elden-ring");
        assert_eq!(s["active"], "elden-ring");
        assert_eq!(s["profiles"][0]["items"].as_object().unwrap().len(), 4);
        assert_eq!(s["version"], crate::VERSION);

        let (_, s) = c.json(M::PUT, "/api/profiles/elden-ring", Some(json!({ "name": "Elden", "include": { "Alerts": false } }))).await;
        assert_eq!(s["profiles"][0]["name"], "Elden");
        assert_eq!(s["profiles"][0]["items"]["Alerts"]["include"], false);

        let (_, s) = c.json(M::POST, "/api/profiles/elden-ring/capture", None).await;
        assert_eq!(s["profiles"][0]["items"]["Alerts"]["include"], false, "re-capture keeps exclusions");

        let (status, _) = c.json(M::POST, "/api/profiles", Some(json!({ "name": "x", "processes": ["bad/name"] }))).await;
        assert_eq!(status, 400);

        let (_, s) = c.json(M::POST, "/api/lock", Some(json!({ "id": "elden-ring" }))).await;
        assert_eq!(s["lock"], "elden-ring");
        let (_, s) = c.json(M::DELETE, "/api/profiles/elden-ring", None).await;
        assert_eq!((s["profiles"].as_array().unwrap().len(), &s["lock"], &s["defaultProfile"]), (0, &Value::Null, &json!("")));

        let foreign = c.http.post(format!("{}/api/lock", c.url)).header("Origin", "https://evil.example").header("Content-Type", "application/json").body(r#"{"id":null}"#).send().await.unwrap();
        assert_eq!(foreign.status(), 403);
        assert_eq!(raw_status(c.port, "evil.example", "/api/state").await, 403, "DNS rebinding: a foreign Host is refused");
        assert_eq!(c.call(M::GET, "/", None).send().await.unwrap().status(), 200);
        assert_eq!(c.call(M::GET, "/../README.md", None).send().await.unwrap().status(), 404);
        let page = c.call(M::GET, "/app.js", None).send().await.unwrap();
        assert_eq!(page.headers()["content-type"], "text/javascript; charset=utf-8");
        obs.stop();
        drop(c.srv);
    }

    #[tokio::test]
    async fn obs_connection_settings_screenshot_and_duplicate() {
        let fake = FakeObs::start(
            "",
            vec![("Live", vec![item(1, "Camera", 10.0, 20.0), item_with(2, "Chat", 1500.0, 100.0, Opts { bounds: "OBS_BOUNDS_SCALE_INNER", ..Opts::default() })])],
        )
        .await;
        let port: u16 = fake.url.rsplit(':').next().unwrap().parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let obs = ObsClient::new(Duration::from_millis(30), Duration::from_secs(5));
        let url = fake.url.clone();
        let read = Arc::new(move || Some(ObsSettings { enabled: true, port, auth_required: false, url: url.clone(), password: String::new() }));
        let connection = Connection::load(obs.clone(), dir.path(), None, None, read);
        let ctl = Controller::new(obs.clone(), store, None);
        ctl.set_connection(connection.clone());
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        let c = serve(ctl.clone(), Some(connection), dir).await;

        let (_, s) = c.json(M::GET, "/api/state", None).await;
        assert_eq!(s["obsInfo"]["source"], "auto");
        assert_eq!(s["obsInfo"]["detected"]["port"], port);

        // Bad input is refused field by field and changes nothing.
        let (status, body) = c.json(M::PUT, "/api/obs", Some(json!({ "mode": "manual", "host": "no spaces please", "port": 0 }))).await;
        assert_eq!(status, 400);
        let mut fields: Vec<String> = body["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap().to_string()).collect();
        fields.sort();
        assert_eq!(fields, ["host", "port"]);
        assert_eq!(obs.url(), fake.url);

        let (status, s) = c.json(M::PUT, "/api/obs", Some(json!({ "mode": "manual", "host": "127.0.0.1", "port": port, "password": "unused" }))).await;
        assert_eq!(status, 200);
        assert_eq!(s["obs"]["state"], "connected", "answers after the reconnect settled");
        assert_eq!(s["obsInfo"]["source"], "manual");
        assert_eq!(s["obsInfo"]["hasPassword"], true);
        assert!(!s.to_string().contains("unused"));

        until(2000, || !ctl.state()["obsVersion"].is_null()).await;
        let (_, s) = c.json(M::POST, "/api/profiles", Some(json!({ "name": "Main" }))).await;
        assert_eq!(s["profiles"][0]["canvas"], json!({ "width": 1920, "height": 1080 }));
        assert_eq!(s["profiles"][0]["items"]["Camera"]["size"], json!({ "w": 1920, "h": 1080 }));
        assert_eq!(s["canvas"], json!({ "width": 1920, "height": 1080 }));
        assert_eq!(s["obsVersion"]["obs"], "31.0.0");

        let (status, s) = c.json(M::POST, "/api/profiles/main/duplicate", None).await;
        assert_eq!(status, 201);
        let summary: Vec<(String, String, usize)> = s["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["id"].as_str().unwrap().into(), p["name"].as_str().unwrap().into(), p["processes"].as_array().unwrap().len()))
            .collect();
        assert_eq!(summary, [("main".into(), "Main".into(), 0), ("main-2".into(), "Main (2)".into(), 0)]);
        assert_eq!(s["profiles"][1]["items"], s["profiles"][0]["items"]);

        let (status, shot) = c.json(M::GET, "/api/screenshot?w=320", None).await;
        assert_eq!(status, 200);
        assert_eq!(shot["scene"], "Live");
        use base64::Engine;
        let b64 = shot["image"].as_str().unwrap().split(',').nth(1).unwrap();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(b64).unwrap(), b"Live@320");

        let (status, apps) = c.json(M::GET, "/api/apps", None).await;
        assert_eq!((status, apps), (200, json!([{ "exe": "game.exe", "title": "Game" }])));
        obs.stop();
    }

    #[tokio::test]
    async fn install_runs_only_with_an_installable_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let ctl = Controller::new(ObsClient::new(Duration::from_millis(30), Duration::from_secs(1)), store, None);
        let c = serve(ctl, None, dir).await;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert_eq!(c.call(M::POST, "/api/update/install", None).send().await.unwrap().status(), 409);
        let h = hits.clone();
        c.srv.set_installer(Arc::new(move || {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        assert_eq!(c.call(M::POST, "/api/update/install", None).send().await.unwrap().status(), 409, "nothing to install");
        c.srv.set_update(json!({ "state": "available", "install": true, "latest": { "version": "9.0.0" } }));
        let (_, s) = c.json(M::GET, "/api/state", None).await;
        assert_eq!(s["update"]["state"], "available");
        assert_eq!(c.call(M::POST, "/api/update/install", None).send().await.unwrap().status(), 202);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
