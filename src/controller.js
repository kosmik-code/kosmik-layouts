// Decides which profile should be on screen and keeps OBS in that state.
// wanted = manual lock ?? detected game ?? default profile. Applied only when *wanted* changes (game started or
// closed, lock, settings) or OBS (re)connects. Never on every poll and never just because OBS differs, so manual
// tweaks, and a layout you just arranged for a game that is not running, stay until the next real switch.
import { EventEmitter } from "node:events";
import { applyLayout, captureLayout, sourceSizes } from "./layout.js";

export class Controller extends EventEmitter {
  constructor({ obs, store, watcher }) {
    super();
    this.obs = obs;
    this.store = store;
    this.watcher = watcher;
    this.detected = null;
    this.lock = null;
    this.active = null; // profile id whose layout OBS shows (last applied or just captured)
    this.appliedFor = null; // the wanted id that was last applied automatically
    this.scenes = [];
    this.log = [];
    this.connection = null; // Connection (src/connection.js): where OBS is, for the setup help on the page
    this.canvas = null; // { width, height } of OBS's base canvas
    this.obsVersion = null; // { obs, websocket } as OBS reports them
    this.sizes = {}; // source name -> { w, h }: unscaled size, so the page can draw layouts saved before sizes were kept
    this.busy = Promise.resolve();
    obs.on("connected", () => {
      this.active = null;
      this.appliedFor = null;
      this.failing = false;
      this.refreshScenes()
        .then(() => this.refreshInfo())
        .finally(() => this.sync("OBS connected"));
    });
    watcher.on("change", (id) => {
      this.detected = id;
      const p = this.profile(id);
      if (p) this.note("detected", { name: p.name }, `Detected ${p.name}`);
      else this.note("noGame", {}, "None of your games is running");
      this.sync("game changed");
    });
  }

  /** Activity entry. key + params let the page show it in its own language; text is the English line for the console. */
  note(key, params, text) {
    this.log.unshift({ at: new Date().toISOString(), key, params, text });
    this.log.length = Math.min(this.log.length, 30);
    this.emit("change");
  }

  profile(id) {
    return this.store.data.profiles.find((p) => p.id === id) ?? null;
  }

  wanted() {
    const d = this.store.data;
    return this.profile(this.lock)?.id ?? this.profile(this.detected)?.id ?? this.profile(d.defaultProfile)?.id ?? null;
  }

  async refreshScenes() {
    try {
      const r = await this.obs.call("GetSceneList");
      this.scenes = (r.scenes ?? []).map((s) => s.sceneName).reverse(); // OBS lists bottom-up
      this.currentProgram = r.currentProgramSceneName ?? null;
    } catch {
      // keep the previous list
    }
  }

  /** Canvas size, version and source sizes. Only for display: failures are ignored. */
  async refreshInfo() {
    try {
      const v = await this.obs.call("GetVideoSettings");
      if (v.baseWidth && v.baseHeight) this.canvas = { width: v.baseWidth, height: v.baseHeight };
    } catch {
      // keep the previous value
    }
    try {
      const v = await this.obs.call("GetVersion");
      this.obsVersion = { obs: v.obsVersion ?? null, websocket: v.obsWebSocketVersion ?? null };
    } catch {
      // older OBS: no version shown
    }
    await this.refreshSizes();
  }

  async refreshSizes() {
    try {
      this.sizes = { ...this.sizes, ...(await sourceSizes(this.obs, await this.sceneName())) };
    } catch {
      // keep the previous sizes
    }
  }

  /** A small JPEG of what the controlled scene shows right now, as a data: URL. */
  async screenshot(width = 640) {
    const scene = await this.sceneName();
    const w = Math.max(160, Math.min(1280, Math.round(Number(width) || 640)));
    const r = await this.obs.call("GetSourceScreenshot", { sourceName: scene, imageFormat: "jpg", imageWidth: w, imageCompressionQuality: 70 });
    return { scene, image: r.imageData };
  }

  /** The scene profiles act on: the chosen one, or whatever is live in OBS right now. */
  async sceneName() {
    if (this.store.data.scene) return this.store.data.scene;
    const r = await this.obs.call("GetCurrentProgramScene");
    return r.currentProgramSceneName ?? r.sceneName;
  }

  /** Serialise OBS work so two switches never interleave their requests. */
  queue(fn) {
    const run = this.busy.then(fn, fn);
    this.busy = run.catch(() => {});
    return run;
  }

  /** Never rejects: a failed switch is logged and retried (OBS answers "not ready" while it is still starting). */
  sync(reason) {
    clearTimeout(this.retryTimer);
    return this.queue(async () => {
      const id = this.wanted();
      if (!id || id === this.appliedFor || !this.obs.connected) return;
      if (id === this.active) {
        this.appliedFor = id; // OBS already shows it (just saved or applied by hand)
        return;
      }
      try {
        if (!this.scenes.length) await this.refreshScenes();
        const r = await this.applyNow(id, reason);
        if (!r.cancelled) this.appliedFor = id;
        this.failing = false;
      } catch (err) {
        const notReady = err.code === 207;
        const name = this.profile(id)?.name ?? id;
        if (!this.failing) {
          if (notReady) this.note("obsStarting", {}, "OBS is still starting, will retry");
          else this.note("applyFailed", { name, error: err.message }, `Could not apply ${name}: ${err.message}`);
        }
        this.failing = true;
        if (this.obs.connected) this.retryTimer = setTimeout(() => this.sync(reason), notReady ? 1000 : 5000).unref();
      }
    });
  }

  async applyNow(id, reason = "manual") {
    const p = this.profile(id);
    if (!p) throw new Error("unknown profile");
    const scene = await this.sceneName();
    const r = await applyLayout(this.obs, scene, p.items, { animateMs: this.store.data.animateMs });
    if (r.cancelled) return r;
    this.active = id;
    this.refreshSizes(); // capture sources change size with the game they show; not awaited, display only
    const miss = r.missing.length ? ` (not in "${scene}": ${r.missing.join(", ")})` : "";
    const aim = r.retargeted?.length ? `; capture switched: ${r.retargeted.join(", ")}` : "";
    this.note("applied", { name: p.name, scene, reason, retargeted: r.retargeted ?? [], missing: r.missing }, `Applied ${p.name} to "${scene}" (${reason})${aim}${miss}`);
    return r;
  }

  setLock(id) {
    this.lock = id && this.profile(id) ? id : null;
    if (this.lock) this.note("locked", { name: this.profile(this.lock).name }, `Locked to ${this.profile(this.lock).name}`);
    else this.note("automatic", {}, "Automatic switching");
    return this.sync(this.lock ? "locked" : "automatic");
  }

  /** Snapshot of the current OBS scene layout. */
  async capture() {
    return this.queue(async () => {
      const scene = await this.sceneName();
      const items = await captureLayout(this.obs, scene);
      for (const [name, it] of Object.entries(items)) if (it.size) this.sizes[name] = it.size;
      return { scene, items, canvas: this.canvas };
    });
  }

  state() {
    const d = this.store.data;
    return {
      obs: this.obs.status,
      obsInfo: this.connection?.view() ?? null,
      obsVersion: this.obsVersion,
      canvas: this.canvas,
      sizes: this.sizes,
      scenes: this.scenes,
      scene: d.scene,
      animateMs: d.animateMs,
      defaultProfile: d.defaultProfile,
      detected: this.detected,
      lock: this.lock,
      active: this.active,
      wanted: this.wanted(),
      profiles: d.profiles,
      log: this.log,
    };
  }
}
