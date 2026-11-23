// Minimal obs-websocket v5 client (built into OBS 28+): handshake with optional password, requests,
// automatic reconnect. Only the few requests this app needs; no event subscriptions.
import { createHash, randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import path from "node:path";
import WebSocket from "ws";

const OP = { Hello: 0, Identify: 1, Identified: 2, Request: 6, RequestResponse: 7 };
const sha256b64 = (s) => createHash("sha256").update(s).digest("base64");

/** obs-websocket auth string: base64(sha256(base64(sha256(password + salt)) + challenge)). */
export function authString(password, salt, challenge) {
  return sha256b64(sha256b64(password + salt) + challenge);
}

/**
 * Port and password from OBS's own settings, so nothing has to be typed or copied.
 * The password stays in this process: it is never written by us or sent to the browser.
 */
export async function readObsSettings(appData = process.env.APPDATA) {
  if (!appData) return null;
  try {
    const raw = await readFile(path.join(appData, "obs-studio", "plugin_config", "obs-websocket", "config.json"), "utf8");
    const c = JSON.parse(raw);
    const port = Number(c.server_port) || 4455;
    const authRequired = c.auth_required !== false;
    return {
      enabled: c.server_enabled !== false,
      port,
      authRequired,
      url: `ws://127.0.0.1:${port}`,
      password: authRequired ? String(c.server_password ?? "") : "",
    };
  } catch {
    return null;
  }
}

export class ObsError extends Error {
  constructor(requestType, status) {
    super(`${requestType} failed: ${status?.comment || `code ${status?.code}`}`);
    this.code = status?.code;
  }
}

/** status: { state: "connecting" | "connected" | "auth_failed" | "disconnected", message } */
export class ObsClient extends EventEmitter {
  constructor({ url, password = "", retryMs = 3000, timeoutMs = 5000, WebSocketImpl = WebSocket } = {}) {
    super();
    this.url = url;
    this.password = password;
    this.retryMs = retryMs;
    this.timeoutMs = timeoutMs;
    this.WebSocketImpl = WebSocketImpl;
    this.ws = null;
    this.pending = new Map();
    this.timer = null;
    this.stopped = true;
    this.status = { state: "disconnected", message: "" };
  }

  get connected() {
    return this.status.state === "connected";
  }

  setStatus(state, message = "") {
    if (state === this.status.state && message === this.status.message) return;
    this.status = { state, message };
    this.emit("status", this.status);
    if (state === "connected") this.emit("connected");
  }

  start() {
    this.stopped = false;
    this.open();
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    this.ws?.terminate();
    this.ws = null;
    this.setStatus("disconnected");
  }

  /** Drop the current connection (if any) and connect again now, e.g. after the address or password changed. */
  restart() {
    this.stopped = false;
    clearTimeout(this.timer);
    const old = this.ws;
    this.ws = null;
    old?.terminate();
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(new Error("OBS connection closed"));
    }
    this.pending.clear();
    if (this.status.state === "connected") this.setStatus("disconnected", "reconnecting");
    this.open();
  }

  open() {
    if (this.stopped) return;
    this.setStatus("connecting");
    let ws;
    try {
      ws = new this.WebSocketImpl(this.url, "obswebsocket.json");
    } catch (err) {
      // A malformed address throws right away instead of emitting "close".
      this.setStatus("disconnected", err.message);
      this.timer = setTimeout(() => this.open(), this.retryMs);
      return;
    }
    this.ws = ws;
    ws.on("message", (data) => this.onMessage(ws, data));
    ws.on("error", () => {});
    ws.on("close", (code, reason) => {
      if (this.ws !== ws) return;
      this.ws = null;
      for (const p of this.pending.values()) p.reject(new Error("OBS connection closed"));
      this.pending.clear();
      // 4009 = authentication failed: retrying with the same password only spams OBS's log.
      if (code === 4009) return this.setStatus("auth_failed", "OBS rejected the WebSocket password");
      this.setStatus("disconnected", String(reason || "OBS is not running or WebSocket server is off"));
      if (!this.stopped) this.timer = setTimeout(() => this.open(), this.retryMs);
    });
  }

  onMessage(ws, data) {
    let msg;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if (msg.op === OP.Hello) {
      const auth = msg.d?.authentication;
      const d = { rpcVersion: 1, eventSubscriptions: 0 };
      if (auth) d.authentication = authString(this.password, auth.salt, auth.challenge);
      ws.send(JSON.stringify({ op: OP.Identify, d }));
    } else if (msg.op === OP.Identified) {
      this.setStatus("connected");
    } else if (msg.op === OP.RequestResponse) {
      const p = this.pending.get(msg.d?.requestId);
      if (!p) return;
      this.pending.delete(msg.d.requestId);
      clearTimeout(p.timer);
      if (msg.d.requestStatus?.result) p.resolve(msg.d.responseData ?? {});
      else p.reject(new ObsError(msg.d.requestType, msg.d.requestStatus));
    }
  }

  /** Send one request; resolves with responseData. */
  call(requestType, requestData = {}) {
    if (!this.connected || !this.ws) return Promise.reject(new Error("OBS is not connected"));
    const requestId = randomUUID();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        reject(new Error(`${requestType} timed out`));
      }, this.timeoutMs);
      this.pending.set(requestId, { resolve, reject, timer });
      this.ws.send(JSON.stringify({ op: OP.Request, d: { requestType, requestId, requestData } }));
    });
  }
}
