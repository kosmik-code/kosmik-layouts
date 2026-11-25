//! Decides which profile should be on screen and keeps OBS in that state.
//! wanted = manual lock ?? detected game ?? default profile. Applied only when *wanted* changes (game
//! started or closed, lock, settings) or OBS (re)connects. Never on every poll and never just because
//! OBS differs, so manual tweaks, and a layout you just arranged for a game that is not running, stay
//! until the next real switch. Same behaviour as src/controller.js.

use crate::connection::Connection;
use crate::games::GameWatcher;
use crate::js::{iso_now, js_number};
use crate::layout::{Applied, apply_layout, capture_layout, source_sizes};
use crate::obs::{ObsClient, ObsError};
use crate::store::Store;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Default)]
struct State {
    detected: Option<String>,
    lock: Option<String>,
    /// Profile id whose layout OBS shows (last applied or just captured).
    active: Option<String>,
    /// The wanted id that was last applied automatically.
    applied_for: Option<String>,
    scenes: Vec<String>,
    log: VecDeque<Value>,
    /// { width, height } of OBS's base canvas.
    canvas: Option<Value>,
    /// { obs, websocket } as OBS reports them.
    obs_version: Option<Value>,
    /// Source name -> { w, h }: unscaled size, so the page can draw layouts saved before sizes were kept.
    sizes: Map<String, Value>,
    failing: bool,
}

pub struct Controller {
    pub obs: ObsClient,
    pub store: Arc<Store>,
    connection: Mutex<Option<Arc<Connection>>>,
    st: Mutex<State>,
    /// Serialises OBS work so two switches never interleave their requests (FIFO, like the JS promise chain).
    queue: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    /// Bumped by every sync: a scheduled retry only runs if nothing synced since.
    retry_gen: AtomicU64,
    changed: watch::Sender<u64>,
    /// (retry_gen, delay ms, reason): a failed sync asks the retry worker to try again later.
    retry_tx: tokio::sync::mpsc::UnboundedSender<(u64, u64, String)>,
    pub animate_frames: u32,
}

impl Controller {
    pub fn new(obs: ObsClient, store: Arc<Store>, watcher: Option<&Arc<GameWatcher>>) -> Arc<Controller> {
        let (changed, _) = watch::channel(0);
        let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, u64, String)>();
        let ctl = Arc::new(Controller {
            obs: obs.clone(),
            store,
            connection: Mutex::new(None),
            st: Mutex::new(State::default()),
            queue: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            retry_gen: AtomicU64::new(0),
            changed,
            retry_tx,
            animate_frames: 12,
        });
        let weak = Arc::downgrade(&ctl);
        tokio::spawn(async move {
            while let Some((g, delay, reason)) = retry_rx.recv().await {
                let weak = weak.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if let Some(me) = weak.upgrade() {
                        if me.retry_gen.load(Ordering::SeqCst) == g {
                            me.sync(&reason).await;
                        }
                    }
                });
            }
        });
        if let Some(w) = watcher {
            let weak = Arc::downgrade(&ctl);
            w.on_change(move |id| {
                if let Some(c) = weak.upgrade() {
                    c.on_detected(id);
                }
            });
        }
        // Each new OBS connection: forget what OBS showed, look around, then put the wanted layout up.
        let weak: Weak<Controller> = Arc::downgrade(&ctl);
        let mut status = obs.status_watch();
        tokio::spawn(async move {
            let mut seen = 0;
            loop {
                let (state, epoch) = {
                    let s = status.borrow_and_update();
                    (s.state.clone(), s.epoch)
                };
                if state == "connected" && epoch != seen {
                    seen = epoch;
                    let Some(c) = weak.upgrade() else { return };
                    c.on_connected();
                }
                if status.changed().await.is_err() {
                    return;
                }
            }
        });
        ctl
    }

    pub fn set_connection(&self, c: Arc<Connection>) {
        *self.connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bumped whenever the activity log or what's on screen changes (the tray follows it).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn on_connected(self: &Arc<Self>) {
        {
            let mut st = self.lock();
            st.active = None;
            st.applied_for = None;
            st.failing = false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.refresh_scenes().await;
            me.refresh_info().await;
            me.sync("OBS connected").await;
        });
    }

    fn on_detected(self: &Arc<Self>, id: Option<String>) {
        self.lock().detected = id.clone();
        match id.as_deref().and_then(|i| self.profile(i)) {
            Some(p) => {
                let name = p["name"].as_str().unwrap_or("").to_string();
                self.note("detected", json!({ "name": name }), &format!("Detected {name}"));
            }
            None => self.note("noGame", json!({}), "None of your games is running"),
        }
        let me = self.clone();
        tokio::spawn(async move { me.sync("game changed").await });
    }

    /// Activity entry. key + params let the page show it in its own language; text is the English line for the log.
    pub fn note(&self, key: &str, params: Value, text: &str) {
        {
            let mut st = self.lock();
            st.log.push_front(json!({ "at": iso_now(), "key": key, "params": params, "text": text }));
            st.log.truncate(30);
        }
        crate::log::line(format!("[layouts] {text}"));
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn profile(&self, id: &str) -> Option<Value> {
        self.store.profile(id)
    }

    fn exists(&self, id: Option<&str>) -> Option<String> {
        id.and_then(|i| self.profile(i)).and_then(|p| p["id"].as_str().map(String::from))
    }

    pub fn wanted(&self) -> Option<String> {
        let (lock, detected) = {
            let st = self.lock();
            (st.lock.clone(), st.detected.clone())
        };
        let def = self.store.with(|d| d["defaultProfile"].as_str().map(String::from));
        self.exists(lock.as_deref()).or_else(|| self.exists(detected.as_deref())).or_else(|| self.exists(def.as_deref()))
    }

    pub fn active(&self) -> Option<String> {
        self.lock().active.clone()
    }
    pub fn set_active(&self, id: Option<String>) {
        self.lock().active = id;
    }
    pub fn lock_id(&self) -> Option<String> {
        self.lock().lock.clone()
    }
    pub fn detected(&self) -> Option<String> {
        self.lock().detected.clone()
    }
    /// After the scene or default changed: apply again on the next sync.
    pub fn forget_applied(&self) {
        let mut st = self.lock();
        st.applied_for = None;
        st.active = None;
    }
    /// A deleted profile can't stay locked or active.
    pub fn forget_profile(&self, id: &str) {
        let mut st = self.lock();
        if st.lock.as_deref() == Some(id) {
            st.lock = None;
        }
        if st.active.as_deref() == Some(id) {
            st.active = None;
        }
    }

    pub async fn refresh_scenes(&self) {
        if let Ok(r) = self.obs.call("GetSceneList", json!({})).await {
            // OBS lists bottom-up.
            let mut scenes: Vec<String> = r["scenes"].as_array().into_iter().flatten().map(|s| crate::js::js_string(&s["sceneName"])).collect();
            scenes.reverse();
            self.lock().scenes = scenes;
        }
    }

    /// Canvas size, version and source sizes. Only for display: failures are ignored.
    pub async fn refresh_info(&self) {
        if let Ok(v) = self.obs.call("GetVideoSettings", json!({})).await {
            if crate::js::truthy(&v["baseWidth"]) && crate::js::truthy(&v["baseHeight"]) {
                self.lock().canvas = Some(json!({ "width": v["baseWidth"], "height": v["baseHeight"] }));
            }
        }
        if let Ok(v) = self.obs.call("GetVersion", json!({})).await {
            let or_null = |x: &Value| if x.is_null() { Value::Null } else { x.clone() };
            self.lock().obs_version = Some(json!({ "obs": or_null(&v["obsVersion"]), "websocket": or_null(&v["obsWebSocketVersion"]) }));
        }
        self.refresh_sizes().await;
    }

    pub async fn refresh_sizes(&self) {
        let Ok(scene) = self.scene_name().await else { return };
        if let Ok(sizes) = source_sizes(&self.obs, &scene).await {
            self.lock().sizes.extend(sizes);
        }
    }

    /// A small JPEG of what the controlled scene shows right now, as a data: URL.
    pub async fn screenshot(&self, width: Option<&str>) -> Result<Value, ObsError> {
        let scene = self.scene_name().await?;
        let n = width.map(|w| js_number(&Value::String(w.into()))).unwrap_or(0.0);
        let n = if n.is_nan() || n == 0.0 { 640.0 } else { n };
        let w = ((n + 0.5).floor()).clamp(160.0, 1280.0) as i64;
        let r = self.obs.call("GetSourceScreenshot", json!({ "sourceName": scene, "imageFormat": "jpg", "imageWidth": w, "imageCompressionQuality": 70 })).await?;
        Ok(json!({ "scene": scene, "image": r["imageData"] }))
    }

    /// The scene profiles act on: the chosen one, or whatever is live in OBS right now.
    pub async fn scene_name(&self) -> Result<String, ObsError> {
        let chosen = self.store.with(|d| d["scene"].as_str().unwrap_or("").to_string());
        if !chosen.is_empty() {
            return Ok(chosen);
        }
        let r = self.obs.call("GetCurrentProgramScene", json!({})).await?;
        let name = if r["currentProgramSceneName"].is_null() { &r["sceneName"] } else { &r["currentProgramSceneName"] };
        Ok(crate::js::js_string(name))
    }

    /// Runs OBS work after whatever is already queued.
    pub async fn queued<T>(&self, f: impl std::future::Future<Output = T>) -> T {
        let _turn = self.queue.lock().await;
        f.await
    }

    /// Waits for queued work to finish (tests).
    pub async fn idle(&self) {
        drop(self.queue.lock().await);
    }

    /// Never fails: a failed switch is logged and retried (OBS answers "not ready" while it is still starting).
    pub async fn sync(self: &Arc<Self>, reason: &str) {
        self.retry_gen.fetch_add(1, Ordering::SeqCst);
        let _turn = self.queue.lock().await;
        let Some(id) = self.wanted() else { return };
        {
            let mut st = self.lock();
            if st.applied_for.as_deref() == Some(id.as_str()) || !self.obs.connected() {
                return;
            }
            if st.active.as_deref() == Some(id.as_str()) {
                st.applied_for = Some(id); // OBS already shows it (just saved or applied by hand)
                return;
            }
        }
        let result = async {
            if self.lock().scenes.is_empty() {
                self.refresh_scenes().await;
            }
            self.apply_now(&id, reason).await
        }
        .await;
        match result {
            Ok(r) => {
                let mut st = self.lock();
                if !r.cancelled {
                    st.applied_for = Some(id);
                }
                st.failing = false;
            }
            Err(err) => {
                let not_ready = err.code == Some(207);
                let name = self.profile(&id).and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_else(|| id.clone());
                let was_failing = std::mem::replace(&mut self.lock().failing, true);
                if !was_failing {
                    if not_ready {
                        self.note("obsStarting", json!({}), "OBS is still starting, will retry");
                    } else {
                        self.note("applyFailed", json!({ "name": name, "error": err.message }), &format!("Could not apply {name}: {}", err.message));
                    }
                }
                if self.obs.connected() {
                    let g = self.retry_gen.load(Ordering::SeqCst);
                    let _ = self.retry_tx.send((g, if not_ready { 1000 } else { 5000 }, reason.to_string()));
                }
            }
        }
    }

    pub async fn apply_now(self: &Arc<Self>, id: &str, reason: &str) -> Result<Applied, ObsError> {
        let Some(p) = self.profile(id) else { return Err(ObsError::msg("unknown profile")) };
        let scene = self.scene_name().await?;
        let animate = self.store.with(|d| d["animateMs"].as_u64().unwrap_or(0));
        let r = apply_layout(&self.obs, &scene, &p["items"], animate, self.animate_frames, &self.generation).await?;
        if r.cancelled {
            return Ok(r);
        }
        self.lock().active = Some(id.to_string());
        // Capture sources change size with the game they show; not awaited, display only.
        let me = self.clone();
        tokio::spawn(async move { me.refresh_sizes().await });
        let name = p["name"].as_str().unwrap_or("").to_string();
        let miss = if r.missing.is_empty() { String::new() } else { format!(" (not in \"{scene}\": {})", r.missing.join(", ")) };
        let aim = if r.retargeted.is_empty() { String::new() } else { format!("; capture switched: {}", r.retargeted.join(", ")) };
        self.note(
            "applied",
            json!({ "name": name, "scene": scene, "reason": reason, "retargeted": r.retargeted, "missing": r.missing }),
            &format!("Applied {name} to \"{scene}\" ({reason}){aim}{miss}"),
        );
        Ok(r)
    }

    pub async fn set_lock(self: &Arc<Self>, id: Option<&str>) {
        let locked = id.filter(|i| !i.is_empty()).and_then(|i| self.profile(i));
        self.lock().lock = locked.as_ref().and_then(|p| p["id"].as_str().map(String::from));
        match locked {
            Some(p) => {
                let name = p["name"].as_str().unwrap_or("").to_string();
                self.note("locked", json!({ "name": name }), &format!("Locked to {name}"));
                self.sync("locked").await;
            }
            None => {
                self.note("automatic", json!({}), "Automatic switching");
                self.sync("automatic").await;
            }
        }
    }

    /// Hotkey: lock the layout on screen (or the one that should be), or back to automatic if locked.
    pub async fn toggle_lock(self: &Arc<Self>) {
        if self.lock_id().is_some() {
            return self.set_lock(None).await;
        }
        if let Some(id) = self.active().or_else(|| self.wanted()) {
            self.set_lock(Some(&id)).await;
        }
    }

    /// Hotkey: step through automatic -> each layout in list order -> automatic again.
    pub async fn lock_next(self: &Arc<Self>) {
        let ids: Vec<String> = self.store.profiles().iter().filter_map(|p| p["id"].as_str().map(String::from)).collect();
        if ids.is_empty() {
            return;
        }
        let next = match self.lock_id().and_then(|l| ids.iter().position(|i| *i == l)) {
            None => ids.first().cloned(),
            Some(i) => ids.get(i + 1).cloned(),
        };
        self.set_lock(next.as_deref()).await;
    }

    /// Snapshot of the current OBS scene layout: (scene, items, canvas).
    pub async fn capture(&self) -> Result<(String, Map<String, Value>, Option<Value>), ObsError> {
        self.queued(async {
            let scene = self.scene_name().await?;
            let items = capture_layout(&self.obs, &scene).await?;
            let mut st = self.lock();
            for (name, it) in &items {
                if let Some(size) = it.get("size") {
                    st.sizes.insert(name.clone(), size.clone());
                }
            }
            Ok((scene, items, st.canvas.clone()))
        })
        .await
    }

    pub fn state(&self) -> Value {
        let d = self.store.data();
        let wanted = self.wanted();
        let conn = self.connection.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let st = self.lock();
        json!({
            "obs": self.obs.status(),
            "obsInfo": conn.map(|c| c.view()),
            "obsVersion": st.obs_version,
            "canvas": st.canvas,
            "sizes": st.sizes,
            "scenes": st.scenes,
            "scene": d["scene"],
            "animateMs": d["animateMs"],
            "defaultProfile": d["defaultProfile"],
            "detected": st.detected,
            "lock": st.lock,
            "active": st.active,
            "wanted": wanted,
            "profiles": d["profiles"],
            "log": st.log,
        })
    }

    /// The latest activity line (tray tooltip).
    pub fn last_note(&self) -> Option<String> {
        self.lock().log.front().and_then(|l| l["text"].as_str().map(String::from))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fake_obs::FakeObs;
    use crate::layout::tests::scene;

    pub async fn until(ms: u64, mut f: impl FnMut() -> bool) {
        let end = std::time::Instant::now() + Duration::from_millis(ms);
        while !f() {
            assert!(std::time::Instant::now() < end, "timed out");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    fn avatar_x(fake: &FakeObs) -> f64 {
        fake.st().item("En Vivo", 1).transform["positionX"].as_f64().unwrap()
    }

    fn set_avatar(fake: &FakeObs, x: f64, y: f64, scale: f64) {
        let mut st = fake.st();
        let t = &mut st.item("En Vivo", 1).transform;
        for (k, v) in [("positionX", x), ("positionY", y), ("scaleX", scale), ("scaleY", scale)] {
            t.insert(k.into(), json!(v));
        }
    }

    fn running_set(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn game_detection_switches_layouts_lock_overrides_default_when_no_game() {
        let fake = FakeObs::start("", scene()).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let obs = ObsClient::new(Duration::from_millis(30), Duration::from_secs(5));
        obs.set_target(&fake.url, "");
        let running = Arc::new(Mutex::new(running_set(&["explorer.exe"])));
        let (s, r) = (store.clone(), running.clone());
        let watcher = GameWatcher::new(Arc::new(move || s.profiles()), Arc::new(move || Ok(r.lock().unwrap().clone())), Duration::from_secs(3));
        let ctl = Controller::new(obs.clone(), store.clone(), Some(&watcher));
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        store.update(|d| Ok(d["animateMs"] = json!(0))).unwrap();

        // Capture two layouts: "Chatting" (avatar big, centre) and "Elden Ring" (avatar small, corner).
        set_avatar(&fake, 960.0, 540.0, 1.0);
        let chatting = ctl.capture().await.unwrap().1;
        set_avatar(&fake, 1700.0, 900.0, 0.3);
        let elden = ctl.capture().await.unwrap().1;
        store
            .update(|d| {
                let p = crate::store::profiles_mut(d);
                p.push(json!({ "id": "chatting", "name": "Chatting", "processes": [], "items": chatting }));
                p.push(json!({ "id": "elden", "name": "Elden Ring", "processes": ["eldenring.exe"], "items": elden }));
                d["defaultProfile"] = json!("chatting");
                Ok(())
            })
            .unwrap();

        watcher.tick().await; // no game -> default
        until(3000, || ctl.active().as_deref() == Some("chatting")).await;
        assert_eq!(avatar_x(&fake), 960.0);

        *running.lock().unwrap() = running_set(&["explorer.exe", "eldenring.exe"]);
        watcher.tick().await;
        until(3000, || ctl.active().as_deref() == Some("elden")).await;
        assert_eq!(avatar_x(&fake), 1700.0);
        assert_eq!(fake.st().item("En Vivo", 1).transform["scaleX"], 0.3);

        // A manual tweak in OBS is not undone by the next poll (same game still running).
        fake.st().item("En Vivo", 1).transform.insert("positionX".into(), json!(1234));
        watcher.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        ctl.idle().await;
        assert_eq!(avatar_x(&fake), 1234.0);

        // Saving a layout for another game while Elden Ring runs does not snap OBS back.
        fake.st().item("En Vivo", 1).transform.insert("positionX".into(), json!(42));
        let other = ctl.capture().await.unwrap().1;
        store
            .update(|d| {
                crate::store::profiles_mut(d).push(json!({ "id": "other", "name": "Other", "processes": ["other.exe"], "items": other }));
                Ok(())
            })
            .unwrap();
        ctl.sync("profiles changed").await;
        assert_eq!(avatar_x(&fake), 42.0);

        ctl.set_lock(Some("chatting")).await;
        until(3000, || ctl.active().as_deref() == Some("chatting")).await;
        assert_eq!(avatar_x(&fake), 960.0);
        *running.lock().unwrap() = running_set(&["explorer.exe"]);
        watcher.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        ctl.idle().await;
        assert_eq!(ctl.active().as_deref(), Some("chatting"));
        ctl.set_lock(None).await;
        assert_eq!(ctl.wanted().as_deref(), Some("chatting"));

        // OBS restarts: the wanted layout is applied again on reconnect.
        fake.st().item("En Vivo", 1).transform.insert("positionX".into(), json!(5));
        fake.drop_clients();
        until(3000, || avatar_x(&fake) == 960.0).await;
        obs.stop();
    }

    #[tokio::test]
    async fn hotkeys_lock_the_layout_on_screen_and_step_through_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let ctl = Controller::new(ObsClient::new(Duration::from_secs(3), Duration::from_secs(5)), store.clone(), None);
        ctl.toggle_lock().await; // no layouts: nothing to lock
        assert_eq!(ctl.lock_id(), None);
        let mut rev = store.revisions();
        store
            .update(|d| {
                let p = crate::store::profiles_mut(d);
                for id in ["a", "b", "c"] {
                    p.push(json!({ "id": id, "name": id.to_uppercase(), "processes": [], "items": {} }));
                }
                d["defaultProfile"] = json!("b");
                Ok(())
            })
            .unwrap();
        assert!(rev.has_changed().unwrap(), "saving bumps the revision");

        ctl.toggle_lock().await; // nothing on screen yet: locks the one that should be (the default)
        assert_eq!(ctl.lock_id().as_deref(), Some("b"));
        ctl.toggle_lock().await;
        assert_eq!(ctl.lock_id(), None);
        ctl.set_active(Some("c".into()));
        ctl.toggle_lock().await; // the layout on screen wins
        assert_eq!(ctl.lock_id().as_deref(), Some("c"));

        let mut seen = vec![];
        for _ in 0..4 {
            ctl.lock_next().await;
            seen.push(ctl.lock_id());
        }
        assert_eq!(seen, vec![None, Some("a".into()), Some("b".into()), Some("c".into())]);
        ctl.lock_next().await;
        assert_eq!(ctl.lock_id(), None, "after the last layout: back to automatic");
    }

    #[tokio::test]
    async fn obs_not_ready_does_not_crash_and_the_layout_is_applied_once_ready() {
        let fake = FakeObs::start("", scene()).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        let obs = ObsClient::new(Duration::from_millis(30), Duration::from_secs(5));
        obs.set_target(&fake.url, "");
        let ctl = Controller::new(obs.clone(), store.clone(), None);
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        store.update(|d| Ok(d["animateMs"] = json!(0))).unwrap();
        let snap = ctl.capture().await.unwrap().1;
        store
            .update(|d| {
                crate::store::profiles_mut(d).push(json!({ "id": "chatting", "name": "Chatting", "processes": [], "items": snap }));
                d["defaultProfile"] = json!("chatting");
                Ok(())
            })
            .unwrap();
        ctl.sync("test").await;

        // OBS restarts: it accepts the connection but refuses requests while loading.
        fake.st().item("En Vivo", 1).transform.insert("positionX".into(), json!(5));
        fake.st().not_ready = true;
        fake.drop_clients();
        until(3000, || ctl.state()["log"].as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap().contains("still starting"))).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(avatar_x(&fake), 5.0, "nothing applied while OBS is not ready");
        fake.st().not_ready = false;
        until(4000, || avatar_x(&fake) == 1600.0).await;
        assert_eq!(ctl.active().as_deref(), Some("chatting"));
        obs.stop();
    }
}
