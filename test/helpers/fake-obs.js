// In-process stand-in for OBS's WebSocket server (protocol v5, with password auth).
// Holds one or more scenes with items; records every request it receives.
import { WebSocketServer } from "ws";
import { authString } from "../../src/obs.js";

// Defaults per input kind, as GetInputDefaultSettings reports them (OBS omits defaults from GetInputSettings).
const DEFAULTS = {
  game_capture: { capture_mode: "any_fullscreen", window: "", priority: 0 },
  window_capture: { window: "", priority: 0, method: 0 },
  image_source: { file: "" },
};

export function item(id, sourceName, x, y, { enabled = true, scale = 1, boundsType = "OBS_BOUNDS_NONE", kind = "image_source", settings = {} } = {}) {
  return {
    sceneItemId: id,
    sourceName,
    inputKind: kind,
    settings, // only non-default values, like real OBS
    sceneItemEnabled: enabled,
    transform: {
      positionX: x, positionY: y, rotation: 0, scaleX: scale, scaleY: scale, alignment: 5,
      boundsType, boundsAlignment: 0, boundsWidth: boundsType === "OBS_BOUNDS_NONE" ? 0 : 640, boundsHeight: boundsType === "OBS_BOUNDS_NONE" ? 0 : 360,
      cropLeft: 0, cropRight: 0, cropTop: 0, cropBottom: 0, sourceWidth: 1920, sourceHeight: 1080, width: 1920 * scale, height: 1080 * scale,
    },
  };
}

export async function startFakeObs({ password = "", scenes = {}, program, canvas = { width: 1920, height: 1080 }, screenshot = null } = {}) {
  const wss = new WebSocketServer({ port: 0, host: "127.0.0.1" });
  await new Promise((r) => wss.once("listening", r));
  // notReady: answer every request with code 207, like OBS does while it is still starting up.
  const state = { scenes, program: program ?? Object.keys(scenes)[0], requests: [], notReady: false };
  const salt = "c2FsdA==";
  const challenge = "Y2hhbGxlbmdl";

  const handlers = {
    GetVersion: () => ({ obsVersion: "31.0.0", obsWebSocketVersion: "5.5.0" }),
    GetVideoSettings: () => ({ baseWidth: canvas.width, baseHeight: canvas.height, outputWidth: canvas.width, outputHeight: canvas.height }),
    GetSourceScreenshot: ({ sourceName, imageWidth }) => {
      if (!state.scenes[sourceName]) throw { code: 600, comment: "No source was found by the name of `sourceName`." };
      if (screenshot) return { imageData: screenshot(sourceName, imageWidth) };
      return { imageData: `data:image/jpg;base64,${Buffer.from(`${sourceName}@${imageWidth}`).toString("base64")}` };
    },
    GetSceneList: () => ({ currentProgramSceneName: state.program, scenes: Object.keys(state.scenes).reverse().map((sceneName, i) => ({ sceneName, sceneIndex: i })) }),
    GetCurrentProgramScene: () => ({ currentProgramSceneName: state.program, sceneName: state.program }),
    GetSceneItemList: ({ sceneName }) => {
      const s = state.scenes[sceneName];
      if (!s) throw { code: 600, comment: "No source was found by the name of `sceneName`." };
      return { sceneItems: s.map((i) => ({ sceneItemId: i.sceneItemId, sourceName: i.sourceName, inputKind: i.inputKind, sceneItemEnabled: i.sceneItemEnabled })) };
    },
    GetInputDefaultSettings: ({ inputKind }) => ({ defaultInputSettings: { ...DEFAULTS[inputKind] } }),
    GetInputSettings: ({ inputName }) => {
      const it = input(inputName);
      return { inputKind: it.inputKind, inputSettings: { ...it.settings } };
    },
    SetInputSettings: ({ inputName, inputSettings, overlay = true }) => {
      const it = input(inputName);
      it.settings = overlay ? { ...it.settings, ...inputSettings } : { ...inputSettings };
      return {};
    },
    GetSceneItemTransform: ({ sceneName, sceneItemId }) => ({ sceneItemTransform: { ...find(sceneName, sceneItemId).transform } }),
    SetSceneItemTransform: ({ sceneName, sceneItemId, sceneItemTransform: t }) => {
      // Same check real OBS does: bounds below 1 are refused.
      if ((t.boundsWidth !== undefined && t.boundsWidth < 1) || (t.boundsHeight !== undefined && t.boundsHeight < 1)) {
        throw { code: 402, comment: "boundsWidth must be at least 1" };
      }
      Object.assign(find(sceneName, sceneItemId).transform, t);
      return {};
    },
    SetSceneItemEnabled: ({ sceneName, sceneItemId, sceneItemEnabled }) => {
      find(sceneName, sceneItemId).sceneItemEnabled = sceneItemEnabled;
      return {};
    },
  };
  // Inputs are global in OBS: the same source in two scenes is one input with one set of settings.
  function input(name) {
    for (const items of Object.values(state.scenes)) {
      const it = items.find((i) => i.sourceName === name);
      if (it) return it;
    }
    throw { code: 600, comment: "No source was found by the name of `inputName`." };
  }
  function find(sceneName, id) {
    const it = state.scenes[sceneName]?.find((i) => i.sceneItemId === id);
    if (!it) throw { code: 600, comment: "item not found" };
    return it;
  }

  wss.on("connection", (ws) => {
    ws.send(JSON.stringify({ op: 0, d: { obsWebSocketVersion: "5.5.0", rpcVersion: 1, ...(password ? { authentication: { salt, challenge } } : {}) } }));
    ws.on("message", (raw) => {
      const msg = JSON.parse(raw);
      if (msg.op === 1) {
        if (password && msg.d.authentication !== authString(password, salt, challenge)) return ws.close(4009, "Authentication failed.");
        return ws.send(JSON.stringify({ op: 2, d: { negotiatedRpcVersion: 1 } }));
      }
      if (msg.op === 6) {
        const { requestType, requestId, requestData } = msg.d;
        state.requests.push({ requestType, requestData });
        let responseData;
        let requestStatus = { result: true, code: 100 };
        try {
          if (state.notReady) throw { code: 207, comment: "OBS is not ready to perform the request." };
          const fn = handlers[requestType];
          if (!fn) throw { code: 204, comment: "unknown request" };
          responseData = fn(requestData ?? {});
        } catch (e) {
          requestStatus = { result: false, code: e.code ?? 500, comment: e.comment ?? String(e) };
        }
        ws.send(JSON.stringify({ op: 7, d: { requestType, requestId, requestStatus, responseData } }));
      }
    });
  });

  return {
    url: `ws://127.0.0.1:${wss.address().port}`,
    state,
    dropClients: () => wss.clients.forEach((c) => c.terminate()),
    close: () => new Promise((r) => { wss.clients.forEach((c) => c.terminate()); wss.close(() => r()); }),
  };
}
