// Local HTTP server for the control page. Loopback only; every request needs a local Host header and
// every change a same-origin Origin header, so other websites can neither read nor drive it.
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { slugify, LIMITS } from "./store.js";

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml" };
const MAX_JSON = 256 * 1024;

class HttpError extends Error {
  constructor(status, message, extra = {}) {
    super(message);
    this.status = status;
    this.body = { error: message, ...extra };
  }
}

function send(res, status, body, headers = {}) {
  const json = body !== undefined && typeof body !== "string" && !Buffer.isBuffer(body);
  res.writeHead(status, {
    "X-Content-Type-Options": "nosniff",
    "Cache-Control": "no-store",
    ...(json ? { "Content-Type": "application/json; charset=utf-8" } : {}),
    ...headers,
  });
  res.end(json ? JSON.stringify(body) : body);
}

async function readJson(req) {
  if (!String(req.headers["content-type"] || "").startsWith("application/json")) throw new HttpError(415, "JSON body required");
  const chunks = [];
  let size = 0;
  for await (const c of req) {
    size += c.length;
    if (size > MAX_JSON) throw new HttpError(413, "payload too large");
    chunks.push(c);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}");
  } catch {
    throw new HttpError(400, "invalid JSON");
  }
}

const cleanProcesses = (list) =>
  [...new Set((Array.isArray(list) ? list : []).map((x) => String(x).trim().toLowerCase()).filter(Boolean))].map((x) => (x.endsWith(".exe") ? x : `${x}.exe`));

export async function startServer({ port = 7430, host = "127.0.0.1", publicDir, controller, store, listApps, connection = null }) {
  let allowedHosts = new Set();
  let allowedOrigins = new Set();

  const save = async (fn, beforeSync) => {
    try {
      const r = await store.update(fn);
      beforeSync?.(r);
      controller.sync("profiles changed");
      return r;
    } catch (err) {
      if (err.errors) throw new HttpError(400, "invalid data", { errors: err.errors });
      throw err;
    }
  };
  const obsCall = async (fn) => {
    if (!controller.obs.connected) throw new HttpError(409, "OBS is not connected");
    try {
      return await fn();
    } catch (err) {
      throw new HttpError(502, err.message);
    }
  };

  async function api(req, res, url) {
    const route = `${req.method} ${url.pathname}`;
    const pm = /^\/api\/profiles\/([a-z0-9-]+)(?:\/(capture|duplicate))?$/.exec(url.pathname);
    const [id, action] = pm ? [pm[1], pm[2] ?? ""] : [];

    if (route === "GET /api/state") return send(res, 200, controller.state());
    if (route === "GET /api/obs") {
      if (!connection) throw new HttpError(404, "not found");
      return send(res, 200, connection.view());
    }
    if (route === "PUT /api/obs") {
      if (!connection) throw new HttpError(404, "not found");
      const r = await connection.save(await readJson(req));
      if (!r.ok) throw new HttpError(400, "invalid connection settings", { errors: r.errors.map((e) => `${e.field}: ${e.message}`), fields: r.errors });
      return send(res, 200, controller.state());
    }
    if (route === "POST /api/obs/reconnect") {
      if (!connection) throw new HttpError(404, "not found");
      connection.apply(true);
      await connection.settled(4000);
      return send(res, 200, controller.state());
    }
    if (route === "GET /api/screenshot") {
      const shot = await obsCall(() => controller.screenshot(url.searchParams.get("w")));
      return send(res, 200, shot);
    }
    if (route === "GET /api/apps") return send(res, 200, await listApps().catch(() => []));
    if (route === "PUT /api/settings") {
      const b = await readJson(req);
      await save((d) => {
        if (b.scene !== undefined) d.scene = String(b.scene);
        if (b.animateMs !== undefined) d.animateMs = Number(b.animateMs);
        if (b.defaultProfile !== undefined) d.defaultProfile = String(b.defaultProfile);
      });
      controller.appliedFor = null; // scene or default may have changed: re-apply
      controller.active = null;
      controller.sync("settings changed");
      return send(res, 200, controller.state());
    }
    if (route === "POST /api/lock") {
      const b = await readJson(req);
      await controller.setLock(b.id || null);
      return send(res, 200, controller.state());
    }
    if (route === "POST /api/profiles") {
      // New profile = the current OBS layout, captured now.
      const b = await readJson(req);
      const { scene, items, canvas } = await obsCall(() => controller.capture());
      const id = await save((d) => {
        const id = slugify(b.name, d.profiles.map((p) => p.id));
        d.profiles.push({ id, name: String(b.name ?? "").trim(), processes: cleanProcesses(b.processes), items, ...(canvas ? { canvas } : {}) });
        if (!d.defaultProfile && b.isDefault) d.defaultProfile = id;
        return id;
      }, (newId) => (controller.active = newId)); // OBS already shows exactly this layout: no need to re-apply it
      const n = Object.keys(items).length;
      controller.note("created", { name: b.name, scene, n }, `Created ${b.name} from "${scene}" (${n} sources)`);
      return send(res, 201, controller.state());
    }
    if (pm && action === "duplicate" && req.method === "POST") {
      // A copy without games, so it never competes with the original; the user adds the game later.
      const src = controller.profile(id);
      if (!src) throw new HttpError(404, "unknown profile");
      const b = await readJson(req).catch(() => ({}));
      const name = String(b.name || `${src.name} (2)`).trim().slice(0, 60);
      await save((d) => {
        if (d.profiles.length >= LIMITS.profiles) throw new HttpError(400, `at most ${LIMITS.profiles} layouts`);
        const copy = structuredClone(src);
        copy.id = slugify(name, d.profiles.map((p) => p.id));
        copy.name = name;
        copy.processes = [];
        d.profiles.splice(d.profiles.findIndex((p) => p.id === id) + 1, 0, copy);
      });
      controller.note("duplicated", { name: src.name, copy: name }, `Duplicated ${src.name} as ${name}`);
      return send(res, 201, controller.state());
    }
    if (pm && action === "capture" && req.method === "POST") {
      if (!controller.profile(id)) throw new HttpError(404, "unknown profile");
      const { scene, items, canvas } = await obsCall(() => controller.capture());
      await save((d) => {
        const p = d.profiles.find((x) => x.id === id);
        // Keep the include/exclude choices the user already made for known sources.
        for (const [name, it] of Object.entries(items)) if (p.items[name]?.include === false) it.include = false;
        p.items = items;
        if (canvas) p.canvas = canvas;
      }, () => (controller.active = id));
      const name = controller.profile(id).name;
      controller.note("recaptured", { name, scene }, `Updated ${name} from "${scene}"`);
      return send(res, 200, controller.state());
    }
    if (pm && !action && req.method === "PUT") {
      const b = await readJson(req);
      await save((d) => {
        const p = d.profiles.find((x) => x.id === id);
        if (!p) throw new HttpError(404, "unknown profile");
        if (b.name !== undefined) p.name = String(b.name).trim();
        if (b.processes !== undefined) p.processes = cleanProcesses(b.processes);
        if (b.include && typeof b.include === "object") {
          for (const [name, on] of Object.entries(b.include)) if (p.items[name]) p.items[name].include = Boolean(on);
        }
      });
      return send(res, 200, controller.state());
    }
    if (pm && !action && req.method === "DELETE") {
      await save((d) => {
        d.profiles = d.profiles.filter((p) => p.id !== id);
        if (d.defaultProfile === id) d.defaultProfile = "";
      });
      if (controller.lock === id) controller.lock = null;
      if (controller.active === id) controller.active = null;
      return send(res, 200, controller.state());
    }
    if (pm && !action && req.method === "POST") {
      // Apply once without locking (preview a profile).
      if (!controller.profile(id)) throw new HttpError(404, "unknown profile");
      await obsCall(() => controller.queue(() => controller.applyNow(id, "applied by hand")));
      return send(res, 200, controller.state());
    }
    throw new HttpError(404, "not found");
  }

  async function handle(req, res) {
    const url = new URL(req.url, "http://local");
    if (!allowedHosts.has(String(req.headers.host || "").toLowerCase())) throw new HttpError(403, "forbidden host");
    if (req.method !== "GET" && req.method !== "HEAD" && !allowedOrigins.has(String(req.headers.origin || "").toLowerCase())) {
      throw new HttpError(403, "forbidden origin");
    }
    if (url.pathname.startsWith("/api/")) return api(req, res, url);
    if (req.method !== "GET") throw new HttpError(405, "method not allowed");
    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    if (!/^[a-z0-9-]+\.(html|js|css|svg)$/.test(name)) throw new HttpError(404, "not found");
    const file = path.join(publicDir, name);
    try {
      if (!(await stat(file)).isFile()) throw new Error();
    } catch {
      throw new HttpError(404, "not found");
    }
    const frame = name.endsWith(".html") ? { "Content-Security-Policy": "frame-ancestors 'self'" } : {};
    send(res, 200, await readFile(file), { "Content-Type": TYPES[path.extname(name)], ...frame });
  }

  const server = http.createServer((req, res) => {
    handle(req, res).catch((err) => {
      if (res.headersSent) return res.destroy();
      if (err instanceof HttpError) return send(res, err.status, err.body);
      console.error("[server]", err);
      send(res, 500, { error: "internal error" });
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, host, resolve);
  });
  const actual = server.address().port;
  allowedHosts = new Set([`127.0.0.1:${actual}`, `localhost:${actual}`]);
  allowedOrigins = new Set([...allowedHosts].map((h) => `http://${h}`));
  return {
    url: `http://127.0.0.1:${actual}`,
    port: actual,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(() => resolve());
      }),
  };
}
