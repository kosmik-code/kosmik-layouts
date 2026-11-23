// data/profiles.json: settings + per-game profiles. Validated before every write; written atomically.
import { readFile, writeFile, rename, mkdir } from "node:fs/promises";
import path from "node:path";

export const LIMITS = { profiles: 50, processes: 10, items: 100 };
const ID = /^[a-z0-9][a-z0-9-]{0,39}$/;
const EXE = /^[^\\/:*?"<>|]{1,80}\.exe$/i;

export function defaultData() {
  return { version: 1, scene: "", animateMs: 400, defaultProfile: "", profiles: [] };
}

export function slugify(name, taken = []) {
  const base =
    String(name ?? "")
      .normalize("NFKD")
      .replace(/[̀-ͯ]/g, "")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 32) || "profile";
  const used = new Set(taken);
  if (!used.has(base)) return base;
  for (let i = 2; ; i++) if (!used.has(`${base}-${i}`)) return `${base}-${i}`;
}

const isObj = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
const isSize = (v, a, b, min = 1) => isObj(v) && [v[a], v[b]].every((n) => Number.isInteger(n) && n >= min && n <= 16384);

/** Returns a list of "path: problem" strings; empty when valid. */
export function validate(d) {
  const e = [];
  if (!isObj(d)) return ["data must be an object"];
  if (typeof d.scene !== "string" || d.scene.length > 200) e.push("scene: must be a scene name");
  if (!Number.isInteger(d.animateMs) || d.animateMs < 0 || d.animateMs > 3000) e.push("animateMs: 0-3000");
  if (!Array.isArray(d.profiles) || d.profiles.length > LIMITS.profiles) return [...e, `profiles: at most ${LIMITS.profiles}`];
  const ids = new Set();
  d.profiles.forEach((p, i) => {
    const at = `profiles[${i}]`;
    if (!isObj(p)) return e.push(`${at}: must be an object`);
    if (typeof p.id !== "string" || !ID.test(p.id)) e.push(`${at}.id: invalid`);
    if (ids.has(p.id)) e.push(`${at}.id: duplicate`);
    ids.add(p.id);
    if (typeof p.name !== "string" || !p.name.trim() || p.name.length > 60) e.push(`${at}.name: 1-60 characters`);
    if (!Array.isArray(p.processes) || p.processes.length > LIMITS.processes || !p.processes.every((x) => typeof x === "string" && EXE.test(x))) {
      e.push(`${at}.processes: up to ${LIMITS.processes} names like game.exe`);
    }
    if (p.canvas !== undefined && !isSize(p.canvas, "width", "height")) e.push(`${at}.canvas: invalid`);
    if (!isObj(p.items) || Object.keys(p.items).length > LIMITS.items) e.push(`${at}.items: at most ${LIMITS.items}`);
    else {
      for (const [name, it] of Object.entries(p.items)) {
        if (!isObj(it) || !isObj(it.transform) || typeof it.visible !== "boolean") e.push(`${at}.items["${name}"]: invalid`);
        else if (it.size !== undefined && !isSize(it.size, "w", "h", 0)) e.push(`${at}.items["${name}"].size: invalid`);
        else if (it.capture !== undefined && (!isObj(it.capture) || typeof it.capture.kind !== "string" || !isObj(it.capture.settings))) {
          e.push(`${at}.items["${name}"].capture: invalid`);
        }
      }
    }
  });
  if (d.defaultProfile !== "" && !ids.has(d.defaultProfile)) e.push("defaultProfile: unknown profile");
  return e;
}

export class Store {
  constructor(dataDir) {
    this.dir = dataDir;
    this.file = path.join(dataDir, "profiles.json");
    this.data = defaultData();
  }

  async load() {
    await mkdir(this.dir, { recursive: true });
    try {
      const parsed = JSON.parse(await readFile(this.file, "utf8"));
      const d = { ...defaultData(), ...parsed };
      if (!validate(d).length) this.data = d;
      else await rename(this.file, path.join(this.dir, `profiles.broken-${Date.now()}.json`));
    } catch (err) {
      if (err.code !== "ENOENT" && !(err instanceof SyntaxError)) throw err;
    }
    return this.data;
  }

  /** Apply fn to a copy; saves only if the result is valid. Throws Error with .errors otherwise. */
  async update(fn) {
    const next = structuredClone(this.data);
    const r = fn(next);
    const errors = validate(next);
    if (errors.length) throw Object.assign(new Error(errors.join("; ")), { errors });
    await mkdir(this.dir, { recursive: true });
    const tmp = `${this.file}.tmp`;
    await writeFile(tmp, JSON.stringify(next, null, 2) + "\n");
    await rename(tmp, this.file);
    this.data = next;
    return r;
  }
}
