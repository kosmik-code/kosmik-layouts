import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { ObsClient, authString, readObsSettings } from "../src/obs.js";
import { startFakeObs, item } from "./helpers/fake-obs.js";

const once = (em, ev, pred = () => true) =>
  new Promise((resolve) => {
    const fn = (v) => {
      if (!pred(v)) return;
      em.off(ev, fn);
      resolve(v);
    };
    em.on(ev, fn);
  });

test("auth string follows the four documented obs-websocket v5 steps", () => {
  // Example inputs from the protocol docs (they give no expected output, so the steps are spelled out here).
  const password = "supersecretpassword";
  const salt = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=";
  const challenge = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";
  const secret = createHash("sha256").update(password + salt).digest().toString("base64"); // steps 1-2
  const expected = createHash("sha256").update(secret + challenge).digest().toString("base64"); // steps 3-4
  assert.equal(authString(password, salt, challenge), expected);
  assert.notEqual(authString("other", salt, challenge), expected);
});

test("connects with the right password and answers requests", async () => {
  const fake = await startFakeObs({ password: "pw", scenes: { Live: [item(1, "Camera", 10, 20)] } });
  const obs = new ObsClient({ url: fake.url, password: "pw", retryMs: 50 });
  obs.start();
  await once(obs, "status", (s) => s.state === "connected");
  const list = await obs.call("GetSceneItemList", { sceneName: "Live" });
  assert.equal(list.sceneItems[0].sourceName, "Camera");
  await assert.rejects(obs.call("GetSceneItemList", { sceneName: "Nope" }), /GetSceneItemList failed/);
  obs.stop();
  await fake.close();
});

test("wrong password stops at auth_failed without retrying", async () => {
  const fake = await startFakeObs({ password: "pw", scenes: { Live: [] } });
  const obs = new ObsClient({ url: fake.url, password: "nope", retryMs: 20 });
  obs.start();
  await once(obs, "status", (s) => s.state === "auth_failed");
  await new Promise((r) => setTimeout(r, 120));
  assert.equal(obs.status.state, "auth_failed");
  obs.stop();
  await fake.close();
});

test("reconnects after OBS drops the connection", async () => {
  const fake = await startFakeObs({ scenes: { Live: [] } });
  const obs = new ObsClient({ url: fake.url, retryMs: 30 });
  obs.start();
  await once(obs, "status", (s) => s.state === "connected");
  fake.dropClients();
  await once(obs, "status", (s) => s.state === "disconnected");
  await once(obs, "status", (s) => s.state === "connected");
  obs.stop();
  await fake.close();
});

test("reads port and password from OBS's own websocket settings", async () => {
  const appData = await mkdtemp(path.join(tmpdir(), "kl-obs-"));
  const dir = path.join(appData, "obs-studio", "plugin_config", "obs-websocket");
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, "config.json"), JSON.stringify({ server_enabled: true, server_port: 4460, auth_required: true, server_password: "s3cret" }));
  assert.deepEqual(await readObsSettings(appData), { enabled: true, port: 4460, authRequired: true, url: "ws://127.0.0.1:4460", password: "s3cret" });
  assert.equal(await readObsSettings(path.join(appData, "missing")), null);
});
