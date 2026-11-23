// Capture the layout of an OBS scene and apply a saved one (optionally animated).
// Items are matched by source name; a profile only touches the items it lists.

const NUMERIC = ["positionX", "positionY", "rotation", "scaleX", "scaleY", "boundsWidth", "boundsHeight"];
const SETTABLE = [...NUMERIC, "alignment", "boundsType", "boundsAlignment", "cropLeft", "cropRight", "cropTop", "cropBottom"];
const NO_BOUNDS = "OBS_BOUNDS_NONE";

/** The part of GetSceneItemTransform that SetSceneItemTransform accepts. */
export function settable(t) {
  const out = {};
  for (const k of SETTABLE) if (t?.[k] !== undefined) out[k] = t[k];
  // OBS rejects bounds smaller than 1 even when bounds are off, so leave them out in that case.
  if (!out.boundsType || out.boundsType === NO_BOUNDS) {
    delete out.boundsWidth;
    delete out.boundsHeight;
  }
  return out;
}

// Capture sources whose *target* (which game/window, which mode) is part of a layout.
// Only these keys are saved and restored; every other setting of the source is left alone.
export const CAPTURE_KINDS = {
  game_capture: ["capture_mode", "window", "priority"],
  window_capture: ["window", "priority", "method"],
  wasapi_process_output_capture: ["window", "priority"], // Application Audio Capture
};
// "Match title, otherwise find window of same executable". OBS's default ("same type") can jump to another
// game built on the same engine (two Unity games share a window class), e.g. an idle game in the background.
export const PRIORITY_EXE = 2;

/** "Title:Class:game.exe" (":" and "#" escaped as #3A / #22) -> its parts. */
export function parseWindow(s) {
  const [title = "", cls = "", exe = ""] = String(s ?? "").split(":").map((p) => p.replace(/#3A/g, ":").replace(/#22/g, "#"));
  return { title, cls, exe };
}

const pick = (obj, keys) => Object.fromEntries(keys.filter((k) => obj?.[k] !== undefined).map((k) => [k, obj[k]]));

/** Current settings including defaults: OBS stores only values that differ from the default, so a source in its
 *  default mode would otherwise have no capture_mode to save or restore. */
async function readSettings(obs, inputName, inputKind) {
  const [{ defaultInputSettings }, { inputSettings }] = await Promise.all([
    obs.call("GetInputDefaultSettings", { inputKind }),
    obs.call("GetInputSettings", { inputName }),
  ]);
  return { ...defaultInputSettings, ...inputSettings };
}

/** The saved capture target of an input, or null for sources that are not window/game captures. */
async function readCapture(obs, inputName, inputKind) {
  const keys = CAPTURE_KINDS[inputKind];
  if (!keys) return null;
  const settings = pick(await readSettings(obs, inputName, inputKind), keys);
  if (settings.window && settings.capture_mode !== "any_fullscreen") settings.priority = PRIORITY_EXE;
  return { kind: inputKind, settings };
}

/** Point capture sources back at the layout's game/window. Only touches sources whose target differs, so an
 *  unchanged capture is never re-hooked (that would flash black). Returns the names it changed. */
async function applyCaptures(obs, jobs) {
  const changed = [];
  for (const { name, capture, kind, visible } of jobs) {
    if (!capture || !visible) continue; // a hidden capture is not on stream: leave it alone, no restart
    if (capture.kind !== kind) continue; // the source was replaced by another type: leave it
    // Window Capture restarts on *every* settings write, so write only when the target really differs.
    const now = await readSettings(obs, name, kind);
    const diff = Object.entries(capture.settings).some(([k, v]) => now[k] !== v);
    if (!diff) continue;
    await obs.call("SetInputSettings", { inputName: name, inputSettings: capture.settings, overlay: true });
    changed.push(name);
  }
  return changed;
}

async function sceneItems(obs, sceneName) {
  const { sceneItems = [] } = await obs.call("GetSceneItemList", { sceneName });
  const byName = new Map();
  for (const it of sceneItems) if (!byName.has(it.sourceName)) byName.set(it.sourceName, it);
  return byName;
}

/** Unscaled source size from a transform (only kept for drawing the layout on the page). 0×0 is kept on purpose:
 *  it marks sources with no picture (audio, a capture with nothing to capture) so the page can leave them out. */
function sizeOf(t) {
  const w = Number(t?.sourceWidth);
  const h = Number(t?.sourceHeight);
  return Number.isFinite(w) && Number.isFinite(h) && w >= 0 && h >= 0 ? { w: Math.round(w), h: Math.round(h) } : null;
}

/** name -> { w, h } for every item of the scene that currently has a size. */
export async function sourceSizes(obs, sceneName) {
  const out = {};
  for (const [name, it] of await sceneItems(obs, sceneName)) {
    const { sceneItemTransform } = await obs.call("GetSceneItemTransform", { sceneName, sceneItemId: it.sceneItemId });
    const size = sizeOf(sceneItemTransform);
    if (size) out[name] = size;
  }
  return out;
}

/** Every top-level item of the scene with its position, size and visibility. */
export async function captureLayout(obs, sceneName) {
  const byName = await sceneItems(obs, sceneName);
  const items = {};
  for (const [name, it] of byName) {
    const { sceneItemTransform } = await obs.call("GetSceneItemTransform", { sceneName, sceneItemId: it.sceneItemId });
    items[name] = { visible: Boolean(it.sceneItemEnabled), transform: settable(sceneItemTransform) };
    const size = sizeOf(sceneItemTransform);
    if (size) items[name].size = size;
    const capture = await readCapture(obs, name, it.inputKind);
    if (capture) items[name].capture = capture;
  }
  return items;
}

const ease = (t) => (t < 0.5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function frame(from, to, k) {
  const out = { ...to };
  for (const key of NUMERIC) {
    if (typeof from[key] === "number" && typeof to[key] === "number") out[key] = from[key] + (to[key] - from[key]) * k;
  }
  return out;
}

let generation = 0;

/**
 * Move the profile's items into place. Returns { applied: [names], missing: [names] }.
 * A newer apply cancels the animation of an older one.
 */
export async function applyLayout(obs, sceneName, items, { animateMs = 0, frames = 12 } = {}) {
  const gen = ++generation;
  const byName = await sceneItems(obs, sceneName);
  const missing = [];
  const jobs = [];
  for (const [name, want] of Object.entries(items)) {
    if (want.include === false) continue;
    const it = byName.get(name);
    if (!it) {
      missing.push(name);
      continue;
    }
    const target = settable(want.transform);
    let from = target;
    if (animateMs > 0) {
      const { sceneItemTransform } = await obs.call("GetSceneItemTransform", { sceneName, sceneItemId: it.sceneItemId });
      from = settable(sceneItemTransform);
      // Switching bounds mode cannot be tweened: jump straight to the target.
      if ((from.boundsType ?? NO_BOUNDS) !== (target.boundsType ?? NO_BOUNDS)) from = target;
    }
    jobs.push({ name, id: it.sceneItemId, kind: it.inputKind, capture: want.capture, from, target, visible: want.visible, wasVisible: Boolean(it.sceneItemEnabled) });
  }

  // Retarget captures before anything slides in, so a source never appears showing the previous game.
  const retargeted = await applyCaptures(obs, jobs);
  if (gen !== generation) return { applied: [], missing, retargeted, cancelled: true };

  const setEnabled = (j, on) => obs.call("SetSceneItemEnabled", { sceneName, sceneItemId: j.id, sceneItemEnabled: on });
  const setTransform = (j, t) => obs.call("SetSceneItemTransform", { sceneName, sceneItemId: j.id, sceneItemTransform: t });

  // Show newly visible items first so they slide in; hide the others after they reach their spot.
  await Promise.all(jobs.filter((j) => j.visible && !j.wasVisible).map((j) => setEnabled(j, true)));
  const steps = animateMs > 0 ? Math.max(1, frames) : 1;
  for (let s = 1; s <= steps; s++) {
    if (gen !== generation) return { applied: [], missing, retargeted, cancelled: true };
    const k = steps === 1 ? 1 : ease(s / steps);
    await Promise.all(jobs.map((j) => setTransform(j, s === steps ? j.target : frame(j.from, j.target, k))));
    if (s < steps) await sleep(animateMs / steps);
  }
  await Promise.all(jobs.filter((j) => !j.visible && j.wasVisible).map((j) => setEnabled(j, false)));
  return { applied: jobs.map((j) => j.name), missing, retargeted };
}
