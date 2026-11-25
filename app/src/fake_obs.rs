//! In-process stand-in for OBS's WebSocket server (protocol v5, with password auth), for tests.
//! Holds scenes with items and records every request it receives. Mirrors test/helpers/fake-obs.js.

use crate::obs::auth_string;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// Only non-default values, like real OBS.
    pub settings: Map<String, Value>,
    pub enabled: bool,
    pub transform: Map<String, Value>,
}

pub struct Opts {
    pub enabled: bool,
    pub scale: f64,
    pub bounds: &'static str,
    pub kind: &'static str,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { enabled: true, scale: 1.0, bounds: "OBS_BOUNDS_NONE", kind: "image_source" }
    }
}

pub fn item(id: i64, name: &str, x: f64, y: f64) -> Item {
    item_with(id, name, x, y, Opts::default())
}

pub fn item_with(id: i64, name: &str, x: f64, y: f64, o: Opts) -> Item {
    let none = o.bounds == "OBS_BOUNDS_NONE";
    let t = json!({
        "positionX": x, "positionY": y, "rotation": 0, "scaleX": o.scale, "scaleY": o.scale, "alignment": 5,
        "boundsType": o.bounds, "boundsAlignment": 0, "boundsWidth": if none { 0 } else { 640 }, "boundsHeight": if none { 0 } else { 360 },
        "cropLeft": 0, "cropRight": 0, "cropTop": 0, "cropBottom": 0, "sourceWidth": 1920, "sourceHeight": 1080,
        "width": 1920.0 * o.scale, "height": 1080.0 * o.scale,
    });
    Item { id, name: name.into(), kind: o.kind.into(), settings: Map::new(), enabled: o.enabled, transform: t.as_object().unwrap().clone() }
}

pub struct State {
    pub scenes: Vec<(String, Vec<Item>)>,
    pub program: String,
    pub requests: Vec<(String, Value)>,
    /// Answer every request with code 207, like OBS while it is still starting up.
    pub not_ready: bool,
}

impl State {
    pub fn scene(&mut self, name: &str) -> &mut Vec<Item> {
        &mut self.scenes.iter_mut().find(|(n, _)| n == name).expect("scene").1
    }
    pub fn item(&mut self, scene: &str, index: usize) -> &mut Item {
        &mut self.scene(scene)[index]
    }
    fn input(&mut self, name: &str) -> Result<&mut Item, (i64, String)> {
        // Inputs are global in OBS: the same source in two scenes is one input with one set of settings.
        self.scenes.iter_mut().flat_map(|(_, items)| items.iter_mut()).find(|i| i.name == name).ok_or((600, "No source was found by the name of `inputName`.".into()))
    }
    fn find(&mut self, scene: &Value, id: &Value) -> Result<&mut Item, (i64, String)> {
        let scene = scene.as_str().unwrap_or("");
        self.scenes
            .iter_mut()
            .find(|(n, _)| n == scene)
            .and_then(|(_, items)| items.iter_mut().find(|i| Some(i.id) == id.as_i64()))
            .ok_or((600, "item not found".into()))
    }
}

fn defaults(kind: &str) -> Value {
    match kind {
        "game_capture" => json!({ "capture_mode": "any_fullscreen", "window": "", "priority": 0 }),
        "window_capture" => json!({ "window": "", "priority": 0, "method": 0 }),
        "image_source" => json!({ "file": "" }),
        _ => json!({}),
    }
}

fn handle(st: &mut State, ty: &str, d: &Value) -> Result<Value, (i64, String)> {
    let canvas = (1920, 1080);
    Ok(match ty {
        "GetVersion" => json!({ "obsVersion": "31.0.0", "obsWebSocketVersion": "5.5.0" }),
        "GetVideoSettings" => json!({ "baseWidth": canvas.0, "baseHeight": canvas.1, "outputWidth": canvas.0, "outputHeight": canvas.1 }),
        "GetSourceScreenshot" => {
            let name = d["sourceName"].as_str().unwrap_or("");
            if !st.scenes.iter().any(|(n, _)| n == name) {
                return Err((600, "No source was found by the name of `sourceName`.".into()));
            }
            use base64::Engine;
            let body = base64::engine::general_purpose::STANDARD.encode(format!("{name}@{}", d["imageWidth"]));
            json!({ "imageData": format!("data:image/jpg;base64,{body}") })
        }
        "GetSceneList" => json!({
            "currentProgramSceneName": st.program,
            "scenes": st.scenes.iter().rev().enumerate().map(|(i, (n, _))| json!({ "sceneName": n, "sceneIndex": i })).collect::<Vec<_>>(),
        }),
        "GetCurrentProgramScene" => json!({ "currentProgramSceneName": st.program, "sceneName": st.program }),
        "GetSceneItemList" => {
            let name = d["sceneName"].as_str().unwrap_or("");
            let Some((_, items)) = st.scenes.iter().find(|(n, _)| n == name) else {
                return Err((600, "No source was found by the name of `sceneName`.".into()));
            };
            json!({ "sceneItems": items.iter().map(|i| json!({ "sceneItemId": i.id, "sourceName": i.name, "inputKind": i.kind, "sceneItemEnabled": i.enabled })).collect::<Vec<_>>() })
        }
        "GetInputDefaultSettings" => json!({ "defaultInputSettings": defaults(d["inputKind"].as_str().unwrap_or("")) }),
        "GetInputSettings" => {
            let it = st.input(d["inputName"].as_str().unwrap_or(""))?;
            json!({ "inputKind": it.kind, "inputSettings": it.settings })
        }
        "SetInputSettings" => {
            let it = st.input(d["inputName"].as_str().unwrap_or(""))?;
            let new = d["inputSettings"].as_object().cloned().unwrap_or_default();
            if d["overlay"] == json!(false) {
                it.settings = new;
            } else {
                it.settings.extend(new);
            }
            json!({})
        }
        "GetSceneItemTransform" => json!({ "sceneItemTransform": st.find(&d["sceneName"], &d["sceneItemId"])?.transform }),
        "SetSceneItemTransform" => {
            let t = &d["sceneItemTransform"];
            // Same check real OBS does: bounds below 1 are refused.
            if ["boundsWidth", "boundsHeight"].iter().any(|k| t[k].as_f64().is_some_and(|v| v < 1.0)) {
                return Err((402, "boundsWidth must be at least 1".into()));
            }
            let it = st.find(&d["sceneName"], &d["sceneItemId"])?;
            it.transform.extend(t.as_object().cloned().unwrap_or_default());
            json!({})
        }
        "SetSceneItemEnabled" => {
            st.find(&d["sceneName"], &d["sceneItemId"])?.enabled = d["sceneItemEnabled"] == json!(true);
            json!({})
        }
        _ => return Err((204, "unknown request".into())),
    })
}

pub struct FakeObs {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    clients: Arc<Mutex<Vec<JoinHandle<()>>>>,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl FakeObs {
    pub async fn start(password: &str, scenes: Vec<(&str, Vec<Item>)>) -> FakeObs {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let program = scenes.first().map(|(n, _)| n.to_string()).unwrap_or_default();
        let state = Arc::new(Mutex::new(State { scenes: scenes.into_iter().map(|(n, i)| (n.to_string(), i)).collect(), program, requests: Vec::new(), not_ready: false }));
        let clients: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let accepted = Arc::new(AtomicUsize::new(0));
        let password = password.to_string();
        let (st, cl, acc) = (state.clone(), clients.clone(), accepted.clone());
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                acc.fetch_add(1, Ordering::SeqCst);
                let (st, password) = (st.clone(), password.clone());
                cl.lock().unwrap().push(tokio::spawn(async move { serve(tcp, st, password).await }));
            }
        });
        FakeObs { url, state, clients, accepted, task }
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn connections(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Cut every client off without a close frame (like OBS quitting).
    pub fn drop_clients(&self) {
        for c in self.clients.lock().unwrap().drain(..) {
            c.abort();
        }
    }

    pub fn requests(&self, ty: &str) -> Vec<Value> {
        self.st().requests.iter().filter(|(t, _)| t == ty).map(|(_, d)| d.clone()).collect()
    }
}

impl Drop for FakeObs {
    fn drop(&mut self) {
        self.task.abort();
        self.drop_clients();
    }
}

async fn serve(tcp: tokio::net::TcpStream, st: Arc<Mutex<State>>, password: String) {
    let cb = |req: &Request, mut res: Response| {
        if req.headers().get("Sec-WebSocket-Protocol").is_some() {
            res.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
        }
        Ok(res)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
    let (salt, challenge) = ("c2FsdA==", "Y2hhbGxlbmdl");
    let mut hello = json!({ "op": 0, "d": { "obsWebSocketVersion": "5.5.0", "rpcVersion": 1 } });
    if !password.is_empty() {
        hello["d"]["authentication"] = json!({ "salt": salt, "challenge": challenge });
    }
    if ws.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    while let Some(Ok(msg)) = ws.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) else { continue };
        match msg["op"].as_u64() {
            Some(1) => {
                if !password.is_empty() && msg["d"]["authentication"].as_str() != Some(auth_string(&password, salt, challenge).as_str()) {
                    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(4009), reason: "Authentication failed.".into() })).await;
                    return;
                }
                let _ = ws.send(Message::Text(json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } }).to_string().into())).await;
            }
            Some(6) => {
                let d = &msg["d"];
                let ty = d["requestType"].as_str().unwrap_or("").to_string();
                let data = if d["requestData"].is_null() { json!({}) } else { d["requestData"].clone() };
                let r = {
                    let mut s = st.lock().unwrap();
                    s.requests.push((ty.clone(), data.clone()));
                    if s.not_ready { Err((207, "OBS is not ready to perform the request.".to_string())) } else { handle(&mut s, &ty, &data) }
                };
                let (status, response) = match r {
                    Ok(v) => (json!({ "result": true, "code": 100 }), v),
                    Err((code, comment)) => (json!({ "result": false, "code": code, "comment": comment }), Value::Null),
                };
                let mut out = json!({ "op": 7, "d": { "requestType": ty, "requestId": d["requestId"], "requestStatus": status } });
                if !response.is_null() {
                    out["d"]["responseData"] = response;
                }
                if ws.send(Message::Text(out.to_string().into())).await.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}
