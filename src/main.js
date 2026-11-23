// Entry point: node src/main.js
// env: KL_PORT (7430), KL_DATA_DIR (./data), KL_OBS_URL / KL_OBS_PASSWORD (override the OBS tab on the page)
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ObsClient } from "./obs.js";
import { Connection } from "./connection.js";
import { Store } from "./store.js";
import { GameWatcher, listWindowedApps } from "./games.js";
import { Controller } from "./controller.js";
import { startServer } from "./server.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const dataDir = path.resolve(process.env.KL_DATA_DIR || path.join(root, "data"));
const port = Number(process.env.KL_PORT || 7430);

const store = new Store(dataDir);
await store.load();

// Address and password: KL_OBS_URL / KL_OBS_PASSWORD, else what was typed on the page, else OBS's own settings.
const obs = new ObsClient({ url: "" });
const connection = await new Connection({ obs, dataDir }).load();
const watcher = new GameWatcher({ getProfiles: () => store.data.profiles });
const controller = new Controller({ obs, store, watcher });
controller.connection = connection;
connection.watch();

let server;
try {
  server = await startServer({ port, publicDir: path.join(root, "public"), controller, store, listApps: listWindowedApps, connection });
} catch (err) {
  if (err.code === "EADDRINUSE") {
    console.error(`Port ${port} is busy. Is Kosmik Layouts already running? Use KL_PORT=<port> to pick another.`);
    process.exit(1);
  }
  throw err;
}

obs.on("status", ({ state, message }) => console.log(`[obs] ${state}${message ? `: ${message}` : ""}`));
controller.on("change", () => console.log(`[layouts] ${controller.log[0]?.text ?? ""}`));
if (connection.source === "auto" && connection.detected?.enabled === false) console.log("[obs] The WebSocket server is off in OBS: Tools › WebSocket Server Settings › Enable.");
console.log(`[obs] Looking for OBS at ${obs.url} (${{ env: "KL_OBS_URL / KL_OBS_PASSWORD", manual: "set on the page", auto: "read from OBS's settings" }[connection.source]})`);
obs.start();
watcher.start();

console.log("Kosmik Layouts running");
console.log(`  Control page: ${server.url}/`);

// Last line of defence: one failed OBS request must never take the whole app down mid-stream.
process.on("unhandledRejection", (err) => console.error(`[error] ${err?.message ?? err}`));

const shutdown = async () => {
  watcher.stop();
  connection.stop();
  obs.stop();
  await server.close();
  process.exit(0);
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
