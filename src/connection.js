// Where OBS is and how to log in. Three sources, strongest first:
//   env     KL_OBS_URL / KL_OBS_PASSWORD (fixed for this run; the page shows it but cannot change it)
//   manual  host, port and password typed on the page (OBS on another PC, a changed port...)
//   auto    read from OBS's own WebSocket settings on this PC (the default: nothing to type)
// Manual values live in data/obs-connection.json. The password never leaves this process: the page only
// learns whether one is saved.
import { readFile, writeFile, rename, mkdir } from "node:fs/promises";
import path from "node:path";
import { readObsSettings } from "./obs.js";

export const DEFAULT_PORT = 4455;
const HOSTNAME = /^(?=.{1,253}$)[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*$/i;
const IPV6 = /^[0-9a-f:.]{2,45}$/i;
const MAX_PASSWORD = 256;

export function defaultManual() {
  return { mode: "auto", host: "127.0.0.1", port: DEFAULT_PORT, password: "" };
}

/** "ws://192.168.1.20:4455" or "192.168.1.20:4455" typed into the host field -> { host, port }. */
export function splitAddress(input) {
  let s = String(input ?? "").trim();
  if (!s) return { host: "" };
  if (!/^[a-z]+:\/\//i.test(s)) s = `ws://${s}`;
  try {
    const u = new URL(s);
    return { host: u.hostname.replace(/^\[|\]$/g, ""), port: u.port ? Number(u.port) : undefined };
  } catch {
    return { host: String(input).trim() };
  }
}

export function isHost(h) {
  return typeof h === "string" && (HOSTNAME.test(h) || (h.includes(":") && IPV6.test(h)));
}

export function wsUrl(host, port) {
  return `ws://${host.includes(":") ? `[${host}]` : host}:${port}`;
}

/** Returns [errors, cleaned]. Errors are { field, message } so the page can show them next to the input. */
export function validateManual(input, previous = defaultManual()) {
  const errors = [];
  const out = { ...previous };
  if (input.mode !== undefined) {
    if (input.mode !== "auto" && input.mode !== "manual") errors.push({ field: "mode", message: "must be auto or manual" });
    else out.mode = input.mode;
  }
  if (input.host !== undefined) {
    const { host, port } = splitAddress(input.host);
    if (!isHost(host)) errors.push({ field: "host", message: "must be an address like 127.0.0.1 or my-pc.local" });
    else out.host = host;
    if (port !== undefined && input.port === undefined) out.port = port;
  }
  if (input.port !== undefined) {
    const port = Number(input.port);
    if (!Number.isInteger(port) || port < 1 || port > 65535) errors.push({ field: "port", message: "must be a number from 1 to 65535" });
    else out.port = port;
  }
  if (input.clearPassword === true) out.password = "";
  else if (typeof input.password === "string" && input.password !== "") {
    if (input.password.length > MAX_PASSWORD) errors.push({ field: "password", message: `at most ${MAX_PASSWORD} characters` });
    else out.password = input.password;
  }
  return [errors, out];
}

export class Connection {
  /**
   * obs: ObsClient whose url/password this keeps up to date.
   * readSettings: reads OBS's own WebSocket settings (injectable for tests).
   */
  constructor({ obs, dataDir, env = process.env, readSettings = readObsSettings, recheckMs = 4000 }) {
    this.obs = obs;
    this.file = path.join(dataDir, "obs-connection.json");
    this.dir = dataDir;
    this.env = { url: env.KL_OBS_URL || "", password: env.KL_OBS_PASSWORD };
    this.readSettings = readSettings;
    this.recheckMs = recheckMs;
    this.manual = defaultManual();
    this.detected = null; // OBS's own settings on this PC, null when not found
    this.timer = null;
  }

  get source() {
    if (this.env.url || this.env.password !== undefined) return "env";
    return this.manual.mode === "manual" ? "manual" : "auto";
  }

  async load() {
    try {
      const saved = JSON.parse(await readFile(this.file, "utf8"));
      const [errors, clean] = validateManual(saved);
      if (!errors.length) this.manual = clean;
    } catch {
      // first run or unreadable: automatic
    }
    this.detected = await this.readSettings().catch(() => null);
    this.apply();
    return this;
  }

  /** What the OBS client should use right now. */
  target() {
    const d = this.detected;
    const autoUrl = d?.url || wsUrl("127.0.0.1", DEFAULT_PORT);
    switch (this.source) {
      case "env":
        return { url: this.env.url || autoUrl, password: this.env.password ?? d?.password ?? "" };
      case "manual":
        return { url: wsUrl(this.manual.host, this.manual.port), password: this.manual.password };
      default:
        return { url: autoUrl, password: d?.password ?? "" };
    }
  }

  /** Point the client at target(); reconnects when something changed (or when forced). */
  apply(force = false) {
    const { url, password } = this.target();
    const changed = url !== this.obs.url || password !== this.obs.password;
    this.obs.url = url;
    this.obs.password = password;
    if ((changed || force) && !this.obs.stopped) this.obs.restart();
    return changed;
  }

  /**
   * Save the page's choices and reconnect. Resolves with { ok, errors } once OBS answered (or after waitMs),
   * so the page can say right away whether the new address and password work.
   */
  async save(input, { waitMs = 4000 } = {}) {
    if (this.source === "env") return { ok: false, errors: [{ field: "mode", message: "set by KL_OBS_URL / KL_OBS_PASSWORD" }] };
    const [errors, next] = validateManual(input ?? {}, this.manual);
    if (errors.length) return { ok: false, errors };
    await mkdir(this.dir, { recursive: true });
    const tmp = `${this.file}.tmp`;
    await writeFile(tmp, JSON.stringify(next, null, 2) + "\n", { mode: 0o600 });
    await rename(tmp, this.file);
    this.manual = next;
    if (next.mode === "auto") this.detected = await this.readSettings().catch(() => null);
    this.apply(true);
    await this.settled(waitMs);
    return { ok: true, errors: [] };
  }

  /** Waits until the client is connected or has clearly failed (wrong password, nobody listening). */
  settled(ms) {
    const done = () => ["connected", "auth_failed"].includes(this.obs.status.state) || (this.obs.status.state === "disconnected" && this.obs.status.message !== "reconnecting");
    if (done()) return Promise.resolve();
    return new Promise((resolve) => {
      const finish = () => {
        clearTimeout(timer);
        this.obs.off("status", check);
        resolve();
      };
      const check = () => done() && finish();
      const timer = setTimeout(finish, ms);
      this.obs.on("status", check);
    });
  }

  /** While not connected in automatic mode, re-read OBS's settings: turning the server on for the first time
   *  creates a new password, and people change ports. That should just work without restarting the app. */
  watch() {
    this.timer = setInterval(async () => {
      if (this.obs.connected || this.source === "manual") return;
      const now = await this.readSettings().catch(() => null);
      this.detected = now;
      // apply() reconnects when the port or password changed; that also un-sticks an auth failure, which
      // does not retry on its own.
      if (this.source === "auto" && now) this.apply();
    }, this.recheckMs);
    this.timer.unref?.();
  }

  stop() {
    clearInterval(this.timer);
  }

  /** Public view for the page. Never includes a password. */
  view() {
    const d = this.detected;
    return {
      source: this.source,
      url: this.obs.url,
      mode: this.manual.mode,
      host: this.manual.host,
      port: this.manual.port,
      hasPassword: Boolean(this.manual.password),
      env: this.source === "env" ? { url: Boolean(this.env.url), password: this.env.password !== undefined } : null,
      detected: d ? { enabled: d.enabled, port: d.port, authRequired: d.authRequired } : null,
    };
  }
}
