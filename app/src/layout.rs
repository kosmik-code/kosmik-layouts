//! Capture the layout of an OBS scene and apply a saved one (optionally animated).
//! Items are matched by source name; a profile only touches the items it lists. Same behaviour as
//! src/layout.js.

use crate::js::{num, strict_eq, truthy};
use crate::obs::{ObsClient, ObsError};
use futures_util::future::try_join_all;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const NUMERIC: [&str; 7] = ["positionX", "positionY", "rotation", "scaleX", "scaleY", "boundsWidth", "boundsHeight"];
/// NUMERIC first, then the rest (the JS order, which is also the saved key order).
const SETTABLE: [&str; 14] = [
    "positionX", "positionY", "rotation", "scaleX", "scaleY", "boundsWidth", "boundsHeight", "alignment", "boundsType", "boundsAlignment", "cropLeft", "cropRight", "cropTop", "cropBottom",
];
const NO_BOUNDS: &str = "OBS_BOUNDS_NONE";

/// The part of GetSceneItemTransform that SetSceneItemTransform accepts.
pub fn settable(t: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for k in SETTABLE.iter() {
        if let Some(v) = t.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    // OBS rejects bounds smaller than 1 even when bounds are off, so leave them out in that case.
    let bounds = out.get("boundsType").cloned().unwrap_or(Value::Null);
    if !truthy(&bounds) || bounds == json!(NO_BOUNDS) {
        out.remove("boundsWidth");
        out.remove("boundsHeight");
    }
    out
}

/// Capture sources whose *target* (which game/window, which mode) is part of a layout.
/// Only these keys are saved and restored; every other setting of the source is left alone.
pub fn capture_keys(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "game_capture" => Some(&["capture_mode", "window", "priority"]),
        "window_capture" => Some(&["window", "priority", "method"]),
        "wasapi_process_output_capture" => Some(&["window", "priority"]), // Application Audio Capture
        _ => None,
    }
}

/// "Match title, otherwise find window of same executable". OBS's default ("same type") can jump to
/// another game built on the same engine (two Unity games share a window class).
pub const PRIORITY_EXE: i64 = 2;

/// Current settings including defaults: OBS stores only values that differ from the default, so a source
/// in its default mode would otherwise have no capture_mode to save or restore.
async fn read_settings(obs: &ObsClient, input_name: &str, input_kind: &str) -> Result<Map<String, Value>, ObsError> {
    let (defaults, current) = tokio::try_join!(
        obs.call("GetInputDefaultSettings", json!({ "inputKind": input_kind })),
        obs.call("GetInputSettings", json!({ "inputName": input_name })),
    )?;
    let mut out = defaults["defaultInputSettings"].as_object().cloned().unwrap_or_default();
    out.extend(current["inputSettings"].as_object().cloned().unwrap_or_default());
    Ok(out)
}

/// The saved capture target of an input, or None for sources that are not window/game captures.
async fn read_capture(obs: &ObsClient, input_name: &str, input_kind: &Value) -> Result<Option<Value>, ObsError> {
    let Some(kind) = input_kind.as_str() else { return Ok(None) };
    let Some(keys) = capture_keys(kind) else { return Ok(None) };
    let all = read_settings(obs, input_name, kind).await?;
    let mut settings = Map::new();
    for k in keys {
        if let Some(v) = all.get(*k) {
            settings.insert((*k).to_string(), v.clone());
        }
    }
    if settings.get("window").is_some_and(truthy) && settings.get("capture_mode") != Some(&json!("any_fullscreen")) {
        settings.insert("priority".into(), json!(PRIORITY_EXE));
    }
    Ok(Some(json!({ "kind": kind, "settings": settings })))
}

/// (sourceName, item) for each top-level item, first one per name, in scene order.
async fn scene_items(obs: &ObsClient, scene: &str) -> Result<Vec<(String, Value)>, ObsError> {
    let r = obs.call("GetSceneItemList", json!({ "sceneName": scene })).await?;
    let mut out: Vec<(String, Value)> = Vec::new();
    for it in r["sceneItems"].as_array().into_iter().flatten() {
        let name = crate::js::js_string(&it["sourceName"]);
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, it.clone()));
        }
    }
    Ok(out)
}

/// Unscaled source size from a transform (only kept for drawing the layout on the page). 0×0 is kept on
/// purpose: it marks sources with no picture (audio, a capture with nothing to capture).
fn size_of(t: &Value) -> Option<Value> {
    let n = |v: &Value| match v {
        Value::Null => Some(0.0),
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    let (w, h) = (n(&t["sourceWidth"])?, n(&t["sourceHeight"])?);
    // Math.round: halves go up.
    (w >= 0.0 && h >= 0.0).then(|| json!({ "w": num((w + 0.5).floor()), "h": num((h + 0.5).floor()) }))
}

async fn transform_of(obs: &ObsClient, scene: &str, it: &Value) -> Result<Value, ObsError> {
    let r = obs.call("GetSceneItemTransform", json!({ "sceneName": scene, "sceneItemId": it["sceneItemId"] })).await?;
    Ok(r["sceneItemTransform"].clone())
}

/// name -> { w, h } for every item of the scene that currently has a size.
pub async fn source_sizes(obs: &ObsClient, scene: &str) -> Result<Map<String, Value>, ObsError> {
    let mut out = Map::new();
    for (name, it) in scene_items(obs, scene).await? {
        if let Some(size) = size_of(&transform_of(obs, scene, &it).await?) {
            out.insert(name, size);
        }
    }
    Ok(out)
}

/// Every top-level item of the scene with its position, size and visibility.
pub async fn capture_layout(obs: &ObsClient, scene: &str) -> Result<Map<String, Value>, ObsError> {
    let mut items = Map::new();
    for (name, it) in scene_items(obs, scene).await? {
        let t = transform_of(obs, scene, &it).await?;
        let mut entry = json!({ "visible": truthy(&it["sceneItemEnabled"]), "transform": settable(&t) });
        if let Some(size) = size_of(&t) {
            entry["size"] = size;
        }
        if let Some(capture) = read_capture(obs, &name, &it["inputKind"]).await? {
            entry["capture"] = capture;
        }
        items.insert(name, entry);
    }
    Ok(items)
}

fn ease(t: f64) -> f64 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn frame(from: &Map<String, Value>, to: &Map<String, Value>, k: f64) -> Map<String, Value> {
    let mut out = to.clone();
    for key in NUMERIC {
        if let (Some(a), Some(b)) = (from.get(key).and_then(Value::as_f64), to.get(key).and_then(Value::as_f64)) {
            out.insert(key.into(), json!(a + (b - a) * k));
        }
    }
    out
}

#[derive(Debug, Default)]
pub struct Applied {
    pub applied: Vec<String>,
    pub missing: Vec<String>,
    pub retargeted: Vec<String>,
    pub cancelled: bool,
}

struct Job {
    name: String,
    id: Value,
    kind: Value,
    capture: Value,
    from: Map<String, Value>,
    target: Map<String, Value>,
    visible: bool,
    was_visible: bool,
}

/// Point capture sources back at the layout's game/window. Only touches sources whose target differs, so
/// an unchanged capture is never re-hooked (that would flash black). Returns the names it changed.
async fn apply_captures(obs: &ObsClient, jobs: &[Job]) -> Result<Vec<String>, ObsError> {
    let mut changed = Vec::new();
    for j in jobs {
        // A hidden capture is not on stream: leave it alone, no restart.
        if !truthy(&j.capture) || !j.visible {
            continue;
        }
        // The source was replaced by another type: leave it.
        if !strict_eq(&j.capture["kind"], &j.kind) {
            continue;
        }
        let Some(kind) = j.kind.as_str() else { continue };
        // Window Capture restarts on *every* settings write, so write only when the target really differs.
        let now = read_settings(obs, &j.name, kind).await?;
        let settings = j.capture["settings"].as_object().cloned().unwrap_or_default();
        let diff = settings.iter().any(|(k, v)| !now.get(k).is_some_and(|n| strict_eq(n, v)));
        if !diff {
            continue;
        }
        obs.call("SetInputSettings", json!({ "inputName": j.name, "inputSettings": settings, "overlay": true })).await?;
        changed.push(j.name.clone());
    }
    Ok(changed)
}

/// Move the profile's items into place. A newer apply (same `generation` counter) cancels the animation
/// of an older one.
pub async fn apply_layout(obs: &ObsClient, scene: &str, items: &Value, animate_ms: u64, frames: u32, generation: &AtomicU64) -> Result<Applied, ObsError> {
    let gen_now = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let by_name = scene_items(obs, scene).await?;
    let mut missing = Vec::new();
    let mut jobs = Vec::new();
    for (name, want) in items.as_object().into_iter().flatten() {
        if want.get("include") == Some(&json!(false)) {
            continue;
        }
        let Some((_, it)) = by_name.iter().find(|(n, _)| n == name) else {
            missing.push(name.clone());
            continue;
        };
        let target = settable(&want["transform"]);
        let mut from = target.clone();
        if animate_ms > 0 {
            from = settable(&transform_of(obs, scene, it).await?);
            // Switching bounds mode cannot be tweened: jump straight to the target.
            let bt = |m: &Map<String, Value>| m.get("boundsType").cloned().filter(|v| !v.is_null()).unwrap_or(json!(NO_BOUNDS));
            if bt(&from) != bt(&target) {
                from = target.clone();
            }
        }
        jobs.push(Job {
            name: name.clone(),
            id: it["sceneItemId"].clone(),
            kind: it["inputKind"].clone(),
            capture: want.get("capture").cloned().unwrap_or(Value::Null),
            from,
            target,
            visible: truthy(&want["visible"]),
            was_visible: truthy(&it["sceneItemEnabled"]),
        });
    }

    let cancelled = |missing: Vec<String>, retargeted: Vec<String>| Applied { applied: vec![], missing, retargeted, cancelled: true };

    // Retarget captures before anything slides in, so a source never appears showing the previous game.
    let retargeted = apply_captures(obs, &jobs).await?;
    if generation.load(Ordering::SeqCst) != gen_now {
        return Ok(cancelled(missing, retargeted));
    }

    let set_enabled = |j: &Job, on: bool| obs.call("SetSceneItemEnabled", json!({ "sceneName": scene, "sceneItemId": j.id, "sceneItemEnabled": on }));
    let set_transform = |j: &Job, t: Map<String, Value>| obs.call("SetSceneItemTransform", json!({ "sceneName": scene, "sceneItemId": j.id, "sceneItemTransform": t }));

    // Show newly visible items first so they slide in; hide the others after they reach their spot.
    try_join_all(jobs.iter().filter(|j| j.visible && !j.was_visible).map(|j| set_enabled(j, true))).await?;
    let steps = if animate_ms > 0 { frames.max(1) } else { 1 };
    for s in 1..=steps {
        if generation.load(Ordering::SeqCst) != gen_now {
            return Ok(cancelled(missing, retargeted));
        }
        let k = if steps == 1 { 1.0 } else { ease(f64::from(s) / f64::from(steps)) };
        try_join_all(jobs.iter().map(|j| set_transform(j, if s == steps { j.target.clone() } else { frame(&j.from, &j.target, k) }))).await?;
        if s < steps {
            tokio::time::sleep(Duration::from_secs_f64(animate_ms as f64 / f64::from(steps) / 1000.0)).await;
        }
    }
    try_join_all(jobs.iter().filter(|j| !j.visible && j.was_visible).map(|j| set_enabled(j, false))).await?;
    Ok(Applied { applied: jobs.iter().map(|j| j.name.clone()).collect(), missing, retargeted, cancelled: false })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fake_obs::{FakeObs, Opts, item, item_with};

    pub fn scene() -> Vec<(&'static str, Vec<crate::fake_obs::Item>)> {
        vec![
            (
                "En Vivo",
                vec![
                    item(1, "Game Capture", 0.0, 0.0),
                    item_with(2, "Avatar", 1600.0, 800.0, Opts { scale: 0.5, ..Opts::default() }),
                    item_with(3, "Chat", 1500.0, 100.0, Opts { bounds: "OBS_BOUNDS_SCALE_INNER", ..Opts::default() }),
                    item_with(4, "Alerts", 960.0, 200.0, Opts { enabled: false, ..Opts::default() }),
                ],
            ),
            ("BRB", vec![item(9, "BG", 0.0, 0.0)]),
        ]
    }

    pub async fn connected(fake: &FakeObs) -> ObsClient {
        let obs = ObsClient::new(Duration::from_millis(30), Duration::from_secs(5));
        obs.set_target(&fake.url, "");
        obs.start();
        assert!(obs.wait_for(3000, |s| s.state == "connected").await);
        obs
    }

    #[tokio::test]
    async fn capture_then_apply_restores_positions_scale_bounds_and_visibility() {
        let fake = FakeObs::start("", scene()).await;
        let obs = connected(&fake).await;
        let snap = Value::Object(capture_layout(&obs, "En Vivo").await.unwrap());
        assert_eq!(snap.as_object().unwrap().keys().collect::<Vec<_>>(), ["Game Capture", "Avatar", "Chat", "Alerts"]);
        assert_eq!(snap["Avatar"]["transform"]["positionX"], 1600.0);
        assert_eq!(snap["Alerts"]["visible"], false);
        assert!(snap["Avatar"]["transform"].get("boundsWidth").is_none(), "no bounds stored when bounds are off");
        assert_eq!(snap["Chat"]["transform"]["boundsWidth"], 640);
        assert!(snap["Avatar"]["transform"].get("width").is_none(), "read-only fields are not stored");

        // Someone rearranges the scene by hand...
        {
            let mut st = fake.st();
            let avatar = st.item("En Vivo", 1);
            for (k, v) in [("positionX", 10.0), ("positionY", 10.0), ("scaleX", 1.0), ("scaleY", 1.0)] {
                avatar.transform.insert(k.into(), json!(v));
            }
            st.item("En Vivo", 3).enabled = true;
            st.item("En Vivo", 0).enabled = false;
        }
        // ...and the saved layout puts it back, animated.
        let r = apply_layout(&obs, "En Vivo", &snap, 60, 4, &AtomicU64::new(0)).await.unwrap();
        assert!(r.missing.is_empty());
        {
            let mut st = fake.st();
            assert_eq!(st.item("En Vivo", 1).transform["positionX"], 1600.0);
            assert_eq!(st.item("En Vivo", 1).transform["scaleX"], 0.5);
            assert!(!st.item("En Vivo", 3).enabled);
            assert!(st.item("En Vivo", 0).enabled);
        }
        let moves: Vec<Value> = fake.requests("SetSceneItemTransform").into_iter().filter(|q| q["sceneItemId"] == 2).collect();
        assert_eq!(moves.len(), 4, "one request per animation frame");
        let x = moves[1]["sceneItemTransform"]["positionX"].as_f64().unwrap();
        assert!(x > 10.0 && x < 1600.0, "in-between frame");
        obs.stop();
    }

    #[tokio::test]
    async fn apply_skips_excluded_items_and_reports_missing_sources() {
        let fake = FakeObs::start("", scene()).await;
        let obs = connected(&fake).await;
        let mut snap = Value::Object(capture_layout(&obs, "En Vivo").await.unwrap());
        snap["Game Capture"]["include"] = json!(false);
        snap["Webcam"] = json!({ "visible": true, "transform": { "positionX": 0, "positionY": 0 } });
        fake.st().requests.clear();
        let r = apply_layout(&obs, "En Vivo", &snap, 0, 12, &AtomicU64::new(0)).await.unwrap();
        assert_eq!(r.missing, ["Webcam"]);
        assert!(!r.applied.contains(&"Game Capture".to_string()));
        assert!(!fake.st().requests.iter().any(|(t, d)| d["sceneItemId"] == 1 && t.starts_with("Set")));
        obs.stop();
    }

    #[tokio::test]
    async fn layouts_remember_which_game_each_capture_points_at_and_switch_it() {
        let poe_win = "Path of Exile:POEWindowClass:PathOfExileSteam.exe";
        let pb_win = "Phantom Blade Zero:UnrealWindow:PhantomBladeZero-Win64-Shipping.exe";
        let fake = FakeObs::start(
            "",
            vec![(
                "En Vivo",
                vec![
                    item_with(1, "Game Capture", 0.0, 0.0, Opts { kind: "game_capture", ..Opts::default() }),
                    item_with(2, "Game Capture Window", 0.0, 0.0, Opts { kind: "window_capture", enabled: false, ..Opts::default() }),
                    item(3, "Avatar", 1600.0, 800.0),
                ],
            )],
        )
        .await;
        let obs = connected(&fake).await;
        let g = AtomicU64::new(0);
        let set = |idx: usize, f: &dyn Fn(&mut crate::fake_obs::Item)| f(fake.st().item("En Vivo", idx));
        let get = |idx: usize| fake.st().item("En Vivo", idx).clone();

        // Just Chatting: game capture left on its default "any fullscreen" mode (OBS stores no capture_mode then).
        let chatting = Value::Object(capture_layout(&obs, "En Vivo").await.unwrap());
        assert_eq!(chatting["Game Capture"]["capture"], json!({ "kind": "game_capture", "settings": { "capture_mode": "any_fullscreen", "window": "", "priority": 0 } }));
        assert!(chatting["Avatar"].get("capture").is_none(), "only capture sources get a target");

        // PoE uses Game Capture on its window; OBS's "same type" priority is saved as "same program".
        set(0, &|i| i.settings = json!({ "capture_mode": "window", "window": poe_win }).as_object().unwrap().clone());
        let poe = Value::Object(capture_layout(&obs, "En Vivo").await.unwrap());
        assert_eq!(poe["Game Capture"]["capture"]["settings"]["priority"], 2);
        assert_eq!(poe["Game Capture"]["capture"]["settings"]["window"], poe_win);

        // Phantom Blade only works with Window Capture: game capture hidden, window capture on the game.
        set(0, &|i| i.enabled = false);
        set(1, &|i| {
            i.enabled = true;
            i.settings = json!({ "window": pb_win, "method": 2 }).as_object().unwrap().clone();
        });
        let mut pb = Value::Object(capture_layout(&obs, "En Vivo").await.unwrap());

        fake.st().requests.clear();
        let r = apply_layout(&obs, "En Vivo", &poe, 0, 12, &g).await.unwrap();
        assert_eq!(r.retargeted, ["Game Capture"]);
        assert_eq!(get(0).settings["window"], poe_win);
        assert!(get(0).enabled && !get(1).enabled);

        let r = apply_layout(&obs, "En Vivo", &poe, 0, 12, &g).await.unwrap();
        assert!(r.retargeted.is_empty(), "an unchanged target is not written again (no re-hook, no black frame)");
        assert_eq!(fake.requests("SetInputSettings").len(), 1);

        set(1, &|i| i.settings = json!({ "window": "Some Idle Game:UnityWndClass:idle.exe", "method": 2, "priority": 0 }).as_object().unwrap().clone());
        let r = apply_layout(&obs, "En Vivo", &pb, 0, 12, &g).await.unwrap();
        assert_eq!(r.retargeted, ["Game Capture Window"]);
        assert_eq!(get(1).settings["window"], pb_win);
        assert_eq!(get(1).settings["priority"], 2);
        assert!(get(1).enabled && !get(0).enabled);

        apply_layout(&obs, "En Vivo", &chatting, 0, 12, &g).await.unwrap();
        assert_eq!(get(0).settings["capture_mode"], "any_fullscreen", "default mode is restored too");

        // An excluded source keeps whatever it points at.
        pb["Game Capture Window"]["include"] = json!(false);
        set(1, &|i| {
            i.settings.insert("window".into(), json!("x:y:other.exe"));
        });
        apply_layout(&obs, "En Vivo", &pb, 0, 12, &g).await.unwrap();
        assert_eq!(get(1).settings["window"], "x:y:other.exe");
        obs.stop();
    }
}
