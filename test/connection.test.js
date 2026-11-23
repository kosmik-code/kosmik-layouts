import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { EventEmitter } from "node:events";
import { ObsClient } from "../src/obs.js";
import { Connection, validateManual, splitAddress, wsUrl } from "../src/connection.js";
import { Store } from "../src/store.js";
import { Controller } from "../src/controller.js";
import { startServer } from "../src/server.js";
import { startFakeObs, item } from "./helpers/fake-obs.js";

const tmp = (p) => mkdtemp(path.join(tmpdir(), p));
const once = (em, ev, pred = () => true) =>
  new Promise((resolve) => {
    const fn = (v) => {
      if (!pred(v)) return;
      em.off(ev, fn);
      resolve(v);
    };
    em.on(ev, fn);
  });

test("manual settings: addresses, ports and passwords are checked", () => {
  assert.deepEqual(splitAddress("ws://192.168.1.20:4460/"), { host: "192.168.1.20", port: 4460 });
  assert.deepEqual(splitAddress("obs-pc.local"), { host: "obs-pc.local", port: undefined });
  assert.equal(wsUrl("::1", 4455), "ws://[::1]:4455");

  let [errors, out] = validateManual({ mode: "manual", host: "192.168.1.20:4460" });
  assert.deepEqual(errors, []);
  assert.deepEqual([out.host, out.port], ["192.168.1.20", 4460], "a port typed with the address is used");
  [errors] = validateManual({ host: "bad host/x" });
  assert.equal(errors[0].field, "host");
  [errors] = validateManual({ port: 70000 });
  assert.equal(errors[0].field, "port");
  [errors] = validateManual({ mode: "remote" });
  assert.equal(errors[0].field, "mode");

  // An empty password field keeps the saved one; clearing it is explicit.
  [, out] = validateManual({ password: "" }, { mode: "manual", host: "h", port: 1, password: "old" });
  assert.equal(out.password, "old");
  [, out] = validateManual({ clearPassword: true }, { mode: "manual", host: "h", port: 1, password: "old" });
  assert.equal(out.password, "");
});

test("automatic mode follows OBS's settings; manual mode wins over them; env wins over both", async () => {
  const dir = await tmp("kl-conn-");
  const detected = { enabled: true, port: 4460, authRequired: true, url: "ws://127.0.0.1:4460", password: "fromobs" };
  const obs = new ObsClient({ url: "" });
  const c = await new Connection({ obs, dataDir: dir, env: {}, readSettings: async () => detected }).load();
  assert.deepEqual([c.source, obs.url, obs.password], ["auto", "ws://127.0.0.1:4460", "fromobs"]);

  await c.save({ mode: "manual", host: "10.0.0.5", port: 4999, password: "typed" }, { waitMs: 0 });
  assert.deepEqual([c.source, obs.url, obs.password], ["manual", "ws://10.0.0.5:4999", "typed"]);
  obs.stop();
  const view = c.view();
  assert.equal(view.hasPassword, true);
  assert.ok(!JSON.stringify(view).includes("typed"), "the page never sees the password");
  assert.ok(!JSON.stringify(view).includes("fromobs"));
  const onDisk = JSON.parse(await readFile(path.join(dir, "obs-connection.json"), "utf8"));
  assert.equal(onDisk.password, "typed");

  // Saved choices survive a restart.
  const obs2 = new ObsClient({ url: "" });
  const c2 = await new Connection({ obs: obs2, dataDir: dir, env: {}, readSettings: async () => detected }).load();
  assert.deepEqual([c2.source, obs2.url, obs2.password], ["manual", "ws://10.0.0.5:4999", "typed"]);

  const obs3 = new ObsClient({ url: "" });
  const c3 = await new Connection({ obs: obs3, dataDir: dir, env: { KL_OBS_URL: "ws://1.2.3.4:1" }, readSettings: async () => detected }).load();
  assert.deepEqual([c3.source, obs3.url, obs3.password], ["env", "ws://1.2.3.4:1", "fromobs"]);
  assert.equal((await c3.save({ mode: "auto" })).ok, false, "env settings can't be changed from the page");
});

test("saving a new password reconnects at once and reports whether OBS accepted it", async (t) => {
  const fake = await startFakeObs({ password: "right", scenes: { Live: [item(1, "Camera", 0, 0)] } });
  const port = Number(new URL(fake.url).port);
  const dir = await tmp("kl-conn-");
  const obs = new ObsClient({ url: "", retryMs: 30 });
  const c = await new Connection({ obs, dataDir: dir, env: {}, readSettings: async () => null }).load();
  t.after(async () => {
    obs.stop();
    await fake.close();
  });
  await c.save({ mode: "manual", host: "127.0.0.1", port, password: "wrong" }, { waitMs: 0 });
  obs.start();
  await once(obs, "status", (s) => s.state === "auth_failed");

  await c.save({ password: "right" }, { waitMs: 3000 });
  assert.equal(obs.status.state, "connected");
  assert.equal(c.view().port, port);
});

test("HTTP API: OBS connection settings, screenshot, duplicate", async (t) => {
  const fake = await startFakeObs({ scenes: { Live: [item(1, "Camera", 10, 20), item(2, "Chat", 1500, 100, { boundsType: "OBS_BOUNDS_SCALE_INNER" })] } });
  const port = Number(new URL(fake.url).port);
  const dir = await tmp("kl-api2-");
  const store = new Store(dir);
  await store.load();
  const obs = new ObsClient({ url: "", retryMs: 30 });
  const connection = await new Connection({ obs, dataDir: dir, env: {}, readSettings: async () => ({ enabled: true, port, authRequired: false, url: fake.url, password: "" }) }).load();
  const ctl = new Controller({ obs, store, watcher: new EventEmitter() });
  ctl.connection = connection;
  obs.start();
  await once(obs, "connected");
  const srv = await startServer({ port: 0, publicDir: path.join(import.meta.dirname, "..", "public"), controller: ctl, store, listApps: async () => [], connection });
  t.after(async () => {
    await srv.close();
    obs.stop();
    await fake.close();
  });
  const call = (p, method = "GET", body) =>
    fetch(srv.url + p, { method, headers: { Origin: srv.url, ...(body ? { "Content-Type": "application/json" } : {}) }, body: body ? JSON.stringify(body) : undefined });

  let s = await (await call("/api/state")).json();
  assert.equal(s.obsInfo.source, "auto");
  assert.equal(s.obsInfo.detected.port, port);

  // Bad input is refused field by field and changes nothing.
  let r = await call("/api/obs", "PUT", { mode: "manual", host: "no spaces please", port: 0 });
  assert.equal(r.status, 400);
  assert.deepEqual((await r.json()).fields.map((f) => f.field).sort(), ["host", "port"]);
  assert.equal(obs.url, fake.url);

  r = await call("/api/obs", "PUT", { mode: "manual", host: "127.0.0.1", port, password: "unused" });
  assert.equal(r.status, 200);
  s = await r.json();
  assert.equal(s.obs.state, "connected", "answers after the reconnect settled");
  assert.equal(s.obsInfo.source, "manual");
  assert.equal(s.obsInfo.hasPassword, true);
  assert.ok(!JSON.stringify(s).includes("unused"));

  for (let i = 0; !ctl.obsVersion && i < 200; i++) await new Promise((res) => setTimeout(res, 10));
  r = await call("/api/profiles", "POST", { name: "Main" });
  s = await r.json();
  assert.deepEqual(s.profiles[0].canvas, { width: 1920, height: 1080 });
  assert.deepEqual(s.profiles[0].items.Camera.size, { w: 1920, h: 1080 });
  assert.deepEqual(s.canvas, { width: 1920, height: 1080 });
  assert.equal(s.obsVersion.obs, "31.0.0");

  r = await call("/api/profiles/main/duplicate", "POST");
  assert.equal(r.status, 201);
  s = await r.json();
  assert.deepEqual(s.profiles.map((p) => [p.id, p.name, p.processes.length]), [["main", "Main", 0], ["main-2", "Main (2)", 0]]);
  assert.deepEqual(s.profiles[1].items, s.profiles[0].items);

  r = await call("/api/screenshot?w=320");
  assert.equal(r.status, 200);
  const shot = await r.json();
  assert.equal(shot.scene, "Live");
  assert.equal(Buffer.from(shot.image.split(",")[1], "base64").toString(), "Live@320");
});
