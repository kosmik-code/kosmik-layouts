// Which game is running: poll the Windows process list and pick the profile whose game started last.
// Cheap (one `tasklist` every few seconds), no admin rights, no injection.
import { execFile } from "node:child_process";
import { EventEmitter } from "node:events";

function run(cmd, args, timeout = 8000) {
  return new Promise((resolve, reject) => {
    execFile(cmd, args, { windowsHide: true, timeout, maxBuffer: 8 * 1024 * 1024 }, (err, stdout) => (err ? reject(err) : resolve(stdout)));
  });
}

/** Lower-case executable names of every running process. */
export async function listProcesses() {
  if (process.platform !== "win32") {
    const out = await run("ps", ["-A", "-o", "comm="]);
    return new Set(out.split("\n").map((l) => l.trim().split("/").pop().toLowerCase()).filter(Boolean));
  }
  const out = await run("tasklist", ["/fo", "csv", "/nh"]);
  const names = new Set();
  for (const line of out.split(/\r?\n/)) {
    const m = /^"([^"]+)"/.exec(line);
    if (m) names.add(m[1].toLowerCase());
  }
  return names;
}

/** Apps with a visible window (for the "pick the game" list). Slower, only used on demand. */
export async function listWindowedApps() {
  if (process.platform !== "win32") return [];
  const script =
    "Get-Process | Where-Object { $_.MainWindowTitle } | ForEach-Object { [pscustomobject]@{ exe = $_.ProcessName + '.exe'; title = $_.MainWindowTitle } } | ConvertTo-Json -Compress";
  const out = await run("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], 15000);
  const parsed = out.trim() ? JSON.parse(out) : [];
  const list = Array.isArray(parsed) ? parsed : [parsed];
  const seen = new Map();
  for (const a of list) {
    const exe = String(a.exe).toLowerCase();
    if (!seen.has(exe)) seen.set(exe, { exe, title: String(a.title).slice(0, 80) });
  }
  return [...seen.values()].sort((a, b) => a.exe.localeCompare(b.exe));
}

/**
 * The profile whose game started most recently; null when none is running. Window focus plays no part, so
 * alt-tabbing to an idle game left open in the background never switches anything. Ties (e.g. everything
 * already running when the app starts) go to the profile listed first.
 * firstSeen: Map exe -> when the watcher first saw it running (optional).
 */
export function matchProfile(profiles, running, firstSeen = new Map()) {
  let best = null;
  let bestAt = -Infinity;
  for (const p of profiles) {
    const seen = p.processes.map((exe) => exe.toLowerCase()).filter((exe) => running.has(exe));
    if (!seen.length) continue;
    const at = Math.max(...seen.map((exe) => firstSeen.get(exe) ?? 0));
    if (at > bestAt) [best, bestAt] = [p, at];
  }
  return best;
}

/** Emits "change" with the matching profile id (or null) whenever it changes. */
export class GameWatcher extends EventEmitter {
  constructor({ getProfiles, intervalMs = 3000, list = listProcesses } = {}) {
    super();
    this.getProfiles = getProfiles;
    this.intervalMs = intervalMs;
    this.list = list;
    this.current = undefined;
    this.timer = null;
    this.firstSeen = new Map();
    this.polls = 0;
  }

  async tick() {
    let running;
    try {
      running = await this.list();
    } catch {
      return; // a failed poll keeps the current profile
    }
    // Remember when each program appeared. Whatever already runs at the first poll counts as "old" (0).
    const now = this.polls++ === 0 ? 0 : Date.now();
    for (const exe of running) if (!this.firstSeen.has(exe)) this.firstSeen.set(exe, now);
    for (const exe of this.firstSeen.keys()) if (!running.has(exe)) this.firstSeen.delete(exe);
    const id = matchProfile(this.getProfiles(), running, this.firstSeen)?.id ?? null;
    if (id !== this.current) {
      this.current = id;
      this.emit("change", id);
    }
  }

  start() {
    const loop = async () => {
      await this.tick();
      this.timer = setTimeout(loop, this.intervalMs);
    };
    loop();
  }

  stop() {
    clearTimeout(this.timer);
  }
}
