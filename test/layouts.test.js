import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { EventEmitter } from "node:events";
import http from "node:http";
import { ObsClient } from "../src/obs.js";
import { captureLayout, applyLayout } from "../src/layout.js";
import { matchProfile, GameWatcher } from "../src/games.js";
import { Store, validate, defaultData, slugify } from "../src/store.js";
import { Controller } from "../src/controller.js";
import { startServer } from "../src/server.js";
import { startFakeObs, item } from "./helpers/fake-obs.js";

const scene = () => ({
  "En Vivo": [item(1, "Game Capture", 0, 0), item(2, "Avatar", 1600, 800, { scale: 0.5 }), item(3, "Chat", 1500, 100, { boundsType: "OBS_BOUNDS_SCALE_INNER" }), item(4, "Alerts", 960, 200, { enabled: false })],
  BRB: [item(9, "BG", 0, 0)],
});

async function connected(fake) {
  const obs = new ObsClient({ url: fake.url, retryMs: 30 });
  const ready = new Promise((r) => obs.on("connected", r));
  obs.start();
  await ready;
  return obs;
}
const until = async (fn, ms = 3000) => {
  const end = Date.now() + ms;
  while (!(await fn())) {
    if (Date.now() > end) assert.fail("timed out");
    await new Promise((r) => setTimeout(r, 15));
  }
};

test("capture then apply restores positions, scale, bounds and visibility", async () => {
  const fake = await startFakeObs({ scenes: scene() });
  const obs = await connected(fake);
  const snap = await captureLayout(obs, "En Vivo");
  assert.deepEqual(Object.keys(snap), ["Game Capture", "Avatar", "Chat", "Alerts"]);
  assert.equal(snap.Avatar.transform.positionX, 1600);
  assert.equal(snap.Alerts.visible, false);
  assert.equal(snap.Avatar.transform.boundsWidth, undefined, "no bounds stored when bounds are off");
  assert.equal(snap.Chat.transform.boundsWidth, 640);
  assert.equal(snap.Avatar.transform.width, undefined, "read-only fields are not stored");

  // Someone rearranges the scene by hand...
  const items = fake.state.scenes["En Vivo"];
  Object.assign(items[1].transform, { positionX: 10, positionY: 10, scaleX: 1, scaleY: 1 });
  items[3].sceneItemEnabled = true;
  items[0].sceneItemEnabled = false;
  // ...and the saved layout puts it back, animated.
  const r = await applyLayout(obs, "En Vivo", snap, { animateMs: 60, frames: 4 });
  assert.deepEqual(r.missing, []);
  assert.equal(items[1].transform.positionX, 1600);
  assert.equal(items[1].transform.scaleX, 0.5);
  assert.equal(items[3].sceneItemEnabled, false);
  assert.equal(items[0].sceneItemEnabled, true);
  const moves = fake.state.requests.filter((q) => q.requestType === "SetSceneItemTransform" && q.requestData.sceneItemId === 2);
  assert.equal(moves.length, 4, "one request per animation frame");
  assert.ok(moves[1].requestData.sceneItemTransform.positionX > 10 && moves[1].requestData.sceneItemTransform.positionX < 1600, "in-between frame");
  obs.stop();
  await fake.close();
});

test("apply skips excluded items and reports sources missing from the scene", async () => {
  const fake = await startFakeObs({ scenes: scene() });
  const obs = await connected(fake);
  const snap = await captureLayout(obs, "En Vivo");
  snap["Game Capture"].include = false;
  snap.Webcam = { visible: true, transform: { positionX: 0, positionY: 0 } };
  fake.state.requests.length = 0;
  const r = await applyLayout(obs, "En Vivo", snap);
  assert.deepEqual(r.missing, ["Webcam"]);
  assert.ok(!r.applied.includes("Game Capture"));
  assert.ok(!fake.state.requests.some((q) => q.requestData?.sceneItemId === 1 && q.requestType.startsWith("Set")));
  obs.stop();
  await fake.close();
});

test("matching is case-insensitive; ties go to the first profile", () => {
  const profiles = [
    { id: "a", processes: ["EldenRing.exe"] },
    { id: "b", processes: ["valorant.exe", "eldenring.exe"] },
  ];
  assert.equal(matchProfile(profiles, new Set(["eldenring.exe", "valorant.exe"])).id, "a");
  assert.equal(matchProfile(profiles, new Set(["valorant.exe"])).id, "b");
  assert.equal(matchProfile(profiles, new Set(["explorer.exe"])), null);
});

test("the game started last wins over an idle game left running in the background", async () => {
  const profiles = [
    { id: "poe", processes: ["pathofexilesteam.exe"] },
    { id: "idle", processes: ["idlegame.exe"] },
  ];
  let running = new Set(["idlegame.exe"]);
  const changes = [];
  const w = new GameWatcher({ getProfiles: () => profiles, list: async () => running });
  w.on("change", (id) => changes.push(id));
  await w.tick(); // idle game was already open when the app started
  await new Promise((r) => setTimeout(r, 5));
  running = new Set(["idlegame.exe", "pathofexilesteam.exe"]);
  await w.tick(); // PoE starts after it
  profiles.reverse();
  await w.tick(); // list order does not matter once start times differ: still PoE
  running = new Set(["idlegame.exe"]);
  await w.tick(); // PoE closes: back to the idle game's layout
  assert.deepEqual(changes, ["idle", "poe", "idle"]);
});

test("store validation and slugs", () => {
  assert.deepEqual(validate(defaultData()), []);
  const bad = { ...defaultData(), defaultProfile: "x", profiles: [{ id: "A B", name: "", processes: ["notexe"], items: {} }] };
  assert.equal(validate(bad).length, 4);
  assert.equal(slugify("Elden Ring: Nightreign!"), "elden-ring-nightreign");
  assert.equal(slugify("Elden Ring", ["elden-ring"]), "elden-ring-2");
});

test("controller: game detection switches layouts; lock overrides; default when no game", async (t) => {
  const fake = await startFakeObs({ scenes: scene() });
  t.after(() => fake.close());
  const store = new Store(await mkdtemp(path.join(tmpdir(), "kl-ctl-")));
  await store.load();
  const obs = new ObsClient({ url: fake.url, retryMs: 30 });
  let running = new Set(["explorer.exe"]);
  const watcher = new GameWatcher({ getProfiles: () => store.data.profiles, list: async () => running });
  const ctl = new Controller({ obs, store, watcher });
  const ready = new Promise((r) => obs.on("connected", r));
  obs.start();
  await ready;
  await store.update((d) => (d.animateMs = 0));

  // Capture two layouts: "Chatting" (avatar big, centre) and "Elden Ring" (avatar small, corner).
  const avatar = () => fake.state.scenes["En Vivo"][1].transform;
  Object.assign(avatar(), { positionX: 960, positionY: 540, scaleX: 1, scaleY: 1 });
  const chatting = (await ctl.capture()).items;
  Object.assign(avatar(), { positionX: 1700, positionY: 900, scaleX: 0.3, scaleY: 0.3 });
  const elden = (await ctl.capture()).items;
  await store.update((d) => {
    d.profiles.push({ id: "chatting", name: "Chatting", processes: [], items: chatting });
    d.profiles.push({ id: "elden", name: "Elden Ring", processes: ["eldenring.exe"], items: elden });
    d.defaultProfile = "chatting";
  });

  await watcher.tick(); // no game -> default
  await ctl.busy;
  await until(() => ctl.active === "chatting");
  assert.equal(avatar().positionX, 960);

  running = new Set(["explorer.exe", "eldenring.exe"]);
  await watcher.tick();
  await until(() => ctl.active === "elden");
  assert.equal(avatar().positionX, 1700);
  assert.equal(avatar().scaleX, 0.3);

  // A manual tweak in OBS is not undone by the next poll (same game still running).
  avatar().positionX = 1234;
  await watcher.tick();
  await ctl.busy;
  assert.equal(avatar().positionX, 1234);

  // Saving a layout for another game while Elden Ring runs does not snap OBS back.
  avatar().positionX = 42;
  const other = (await ctl.capture()).items;
  await store.update((d) => d.profiles.push({ id: "other", name: "Other", processes: ["other.exe"], items: other }));
  await ctl.sync("profiles changed");
  assert.equal(avatar().positionX, 42);

  await ctl.setLock("chatting");
  await until(() => ctl.active === "chatting");
  assert.equal(avatar().positionX, 960);
  running = new Set(["explorer.exe"]);
  await watcher.tick();
  await ctl.busy;
  assert.equal(ctl.active, "chatting");
  await ctl.setLock(null);
  assert.equal(ctl.wanted(), "chatting");

  // OBS restarts: the wanted layout is applied again on reconnect.
  avatar().positionX = 5;
  fake.dropClients();
  await until(() => avatar().positionX === 960);
  obs.stop();
});

test("layouts remember which game each capture source points at, and switch it", async (t) => {
  const poeWin = "Path of Exile:POEWindowClass:PathOfExileSteam.exe";
  const pbWin = "Phantom Blade Zero:UnrealWindow:PhantomBladeZero-Win64-Shipping.exe";
  const fake = await startFakeObs({
    scenes: {
      "En Vivo": [
        item(1, "Game Capture", 0, 0, { kind: "game_capture" }),
        item(2, "Game Capture Window", 0, 0, { kind: "window_capture", enabled: false }),
        item(3, "Avatar", 1600, 800),
      ],
    },
  });
  t.after(() => fake.close());
  const obs = await connected(fake);
  t.after(() => obs.stop());
  const [gc, wc] = fake.state.scenes["En Vivo"];

  // Just Chatting: game capture left on its default "any fullscreen" mode (OBS stores no capture_mode then).
  const chatting = await captureLayout(obs, "En Vivo");
  assert.deepEqual(chatting["Game Capture"].capture, { kind: "game_capture", settings: { capture_mode: "any_fullscreen", window: "", priority: 0 } });
  assert.equal(chatting.Avatar.capture, undefined, "only capture sources get a target");

  // PoE uses Game Capture on its window; OBS's "same type" priority is saved as "same program".
  gc.settings = { capture_mode: "window", window: poeWin };
  const poe = await captureLayout(obs, "En Vivo");
  assert.equal(poe["Game Capture"].capture.settings.priority, 2);
  assert.equal(poe["Game Capture"].capture.settings.window, poeWin);

  // Phantom Blade only works with Window Capture: game capture hidden, window capture on the game.
  gc.sceneItemEnabled = false;
  wc.sceneItemEnabled = true;
  wc.settings = { window: pbWin, method: 2 };
  const pb = await captureLayout(obs, "En Vivo");

  const sets = () => fake.state.requests.filter((q) => q.requestType === "SetInputSettings");
  fake.state.requests.length = 0;
  let r = await applyLayout(obs, "En Vivo", poe);
  assert.deepEqual(r.retargeted, ["Game Capture"]);
  assert.equal(gc.settings.window, poeWin);
  assert.equal(gc.sceneItemEnabled, true);
  assert.equal(wc.sceneItemEnabled, false);

  r = await applyLayout(obs, "En Vivo", poe);
  assert.deepEqual(r.retargeted, [], "an unchanged target is not written again (no re-hook, no black frame)");
  assert.equal(sets().length, 1);

  wc.settings = { window: "Some Idle Game:UnityWndClass:idle.exe", method: 2, priority: 0 };
  r = await applyLayout(obs, "En Vivo", pb);
  assert.deepEqual(r.retargeted, ["Game Capture Window"]);
  assert.equal(wc.settings.window, pbWin);
  assert.equal(wc.settings.priority, 2);
  assert.equal(wc.sceneItemEnabled, true);
  assert.equal(gc.sceneItemEnabled, false);

  r = await applyLayout(obs, "En Vivo", chatting);
  assert.equal(gc.settings.capture_mode, "any_fullscreen", "default mode is restored too");

  // An excluded source keeps whatever it points at.
  pb["Game Capture Window"].include = false;
  wc.settings.window = "x:y:other.exe";
  r = await applyLayout(obs, "En Vivo", pb);
  assert.equal(wc.settings.window, "x:y:other.exe");
});

test("OBS restarting and 'not ready' does not crash; the layout is applied once OBS is ready", async (t) => {
  const fake = await startFakeObs({ scenes: scene() });
  t.after(() => fake.close());
  const store = new Store(await mkdtemp(path.join(tmpdir(), "kl-ready-")));
  await store.load();
  const obs = new ObsClient({ url: fake.url, retryMs: 30 });
  t.after(() => obs.stop());
  const ctl = new Controller({ obs, store, watcher: new EventEmitter() });
  const ready = new Promise((r) => obs.on("connected", r));
  obs.start();
  await ready;
  await store.update((d) => (d.animateMs = 0));
  const avatar = () => fake.state.scenes["En Vivo"][1].transform;
  const snap = (await ctl.capture()).items;
  await store.update((d) => {
    d.profiles.push({ id: "chatting", name: "Chatting", processes: [], items: snap });
    d.defaultProfile = "chatting";
  });
  await ctl.sync("test");

  // OBS restarts: it accepts the connection but refuses requests while loading.
  avatar().positionX = 5;
  fake.state.notReady = true;
  fake.dropClients();
  await until(() => ctl.log.some((l) => l.text.includes("still starting")));
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(avatar().positionX, 5, "nothing applied while OBS is not ready");
  fake.state.notReady = false;
  await until(() => avatar().positionX === 1600, 4000);
  assert.equal(ctl.active, "chatting");
});

test("HTTP API: create from OBS, rename, exclude a source, lock, delete; foreign origin refused", async (t) => {
  const fake = await startFakeObs({ scenes: scene() });
  const store = new Store(await mkdtemp(path.join(tmpdir(), "kl-api-")));
  await store.load();
  const obs = new ObsClient({ url: fake.url, retryMs: 30 });
  const watcher = new EventEmitter();
  const ctl = new Controller({ obs, store, watcher });
  const ready = new Promise((r) => obs.on("connected", r));
  obs.start();
  await ready;
  const srv = await startServer({ port: 0, publicDir: path.join(import.meta.dirname, "..", "public"), controller: ctl, store, listApps: async () => [{ exe: "game.exe", title: "Game" }] });
  t.after(async () => {
    await srv.close();
    obs.stop();
    await fake.close();
  });
  const call = (p, method = "GET", body, origin = srv.url) =>
    fetch(srv.url + p, { method, headers: { Origin: origin, ...(body ? { "Content-Type": "application/json" } : {}) }, body: body ? JSON.stringify(body) : undefined });

  let r = await call("/api/profiles", "POST", { name: "Elden Ring", processes: ["EldenRing"], isDefault: true });
  assert.equal(r.status, 201);
  let s = await r.json();
  assert.deepEqual(s.profiles[0].processes, ["eldenring.exe"]);
  assert.equal(s.profiles[0].id, "elden-ring");
  assert.equal(s.defaultProfile, "elden-ring");
  assert.equal(s.active, "elden-ring");
  assert.equal(Object.keys(s.profiles[0].items).length, 4);

  r = await call("/api/profiles/elden-ring", "PUT", { name: "Elden", include: { Alerts: false } });
  s = await r.json();
  assert.equal(s.profiles[0].name, "Elden");
  assert.equal(s.profiles[0].items.Alerts.include, false);

  r = await call("/api/profiles/elden-ring/capture", "POST");
  s = await r.json();
  assert.equal(s.profiles[0].items.Alerts.include, false, "re-capture keeps exclusions");

  r = await call("/api/profiles", "POST", { name: "x", processes: ["bad/name"] });
  assert.equal(r.status, 400);

  r = await call("/api/lock", "POST", { id: "elden-ring" });
  assert.equal((await r.json()).lock, "elden-ring");
  r = await call("/api/profiles/elden-ring", "DELETE");
  s = await r.json();
  assert.deepEqual([s.profiles.length, s.lock, s.defaultProfile], [0, null, ""]);

  assert.equal((await call("/api/lock", "POST", { id: null }, "https://evil.example")).status, 403);
  // fetch() cannot override Host, so DNS rebinding is simulated with a raw request.
  const hostStatus = await new Promise((resolve, reject) => {
    http.get({ host: "127.0.0.1", port: srv.port, path: "/api/state", headers: { Host: "evil.example" } }, (res) => {
      res.resume();
      resolve(res.statusCode);
    }).on("error", reject);
  });
  assert.equal(hostStatus, 403);
  assert.equal((await call("/")).status, 200);
  assert.equal((await call("/../package.json")).status, 404);
});
