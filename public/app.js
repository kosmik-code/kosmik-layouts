// Control page: polls /api/state and drives the API. No framework, no build step.
import { t, has, initialLanguage, setLanguage, getLanguage } from "/i18n.js";

const $ = (s, r = document) => r.querySelector(s);
const SVG = "http://www.w3.org/2000/svg";
let S = null;
let profilesSig = "";
let serverVersion = null; // the app version first seen; a new one after an update reloads the page
let updateSig = "";
const UPDATE_DISMISS_KEY = "kl-update-dismissed";
let tab = "layouts";

function h(tag, props = {}, ...kids) {
  const el = tag.startsWith("svg:") ? document.createElementNS(SVG, tag.slice(4)) : document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) continue;
    if (k === "class") el.setAttribute("class", v);
    else if (k === "text") el.textContent = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "checked" || k === "value") el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid);
  return el;
}

class ApiError extends Error {
  constructor(message, data) {
    super(message);
    this.data = data;
  }
}

async function api(path, method = "GET", body) {
  const res = await fetch(path, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(data.errors?.join("; ") || data.error || `HTTP ${res.status}`, data);
  if (data.profiles) apply(data);
  return data;
}

const store = {
  get(k) {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(k, v);
    } catch {
      // not remembered
    }
  },
};

const nameOf = (id) => S?.profiles.find((p) => p.id === id)?.name ?? "—";
const TONE = { connected: "ok", connecting: "warn", disconnected: "bad", auth_failed: "bad" };
const online = () => S?.obs.state === "connected";

function flash(el, text, tone = "ok") {
  el.hidden = false;
  el.className = `msg tone-${tone}`;
  el.textContent = text;
}

function toast(text, tone = "ok") {
  const box = $("#toast");
  box.textContent = text;
  box.className = `toast tone-${tone}`;
  box.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (box.hidden = true), tone === "ok" ? 3000 : 6000);
}

// ---------------------------------------------------------------- language

function applyLanguage(lang, remember) {
  setLanguage(lang, remember);
  document.querySelectorAll("[data-i18n]").forEach((el) => (el.textContent = t(el.dataset.i18n)));
  document.querySelectorAll("[data-i18n-attr]").forEach((el) => {
    for (const pair of el.dataset.i18nAttr.split(";")) {
      const [attr, key] = pair.split(":");
      el.setAttribute(attr, t(key));
    }
  });
  document.querySelectorAll("#lang-switch button").forEach((b) => {
    const on = b.dataset.lang === getLanguage();
    b.classList.toggle("active", on);
    b.setAttribute("aria-pressed", String(on));
  });
  window.KosmikUI?.mountLooks($("#looks"), { studio: t("look.studio"), daylight: t("look.daylight"), pixel: t("look.pixel") });
  renderLiveToggle();
  if (S) {
    profilesSig = "";
    $("#scene").dataset.sig = "";
    $("#default").dataset.sig = "";
    $("#conn-msg").hidden = true; // a message in the old language; the form keeps what was typed
    $("#conn-pass-show").textContent = t($("#conn-pass").type === "text" ? "conn.hide" : "conn.show");
    apply(S);
  }
}
document.querySelectorAll("#lang-switch button").forEach((b) => b.addEventListener("click", () => applyLanguage(b.dataset.lang, true)));

// ---------------------------------------------------------------- tabs

const TABS = ["layouts", "obs"];
function setTab(next, focus = false) {
  tab = TABS.includes(next) ? next : "layouts";
  document.querySelectorAll("[data-panel]").forEach((p) => (p.hidden = p.dataset.panel !== tab));
  document.querySelectorAll(".tabs a").forEach((a) => {
    const on = a.dataset.tab === tab;
    a.classList.toggle("active", on);
    if (on) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  if (tab === "obs") {
    connFilled = false;
    if (S) renderConn();
  }
  if (focus) document.querySelector(`[data-panel="${tab}"] h2`)?.scrollIntoView({ block: "nearest" });
  liveTick();
}
function goTo(next) {
  if (location.hash !== `#${next}`) history.replaceState(null, "", `#${next}`);
  setTab(next, true);
}
window.addEventListener("hashchange", () => setTab(location.hash.slice(1)));
document.querySelectorAll(".tabs a").forEach((a) =>
  a.addEventListener("click", (e) => {
    e.preventDefault();
    goTo(a.dataset.tab);
  }),
);

// ---------------------------------------------------------------- state

function apply(state) {
  if (state.version) {
    // After an update the app restarts with a new page: load it.
    if (serverVersion && state.version !== serverVersion) return location.reload();
    serverVersion = state.version;
  }
  const was = S?.obs.state;
  S = state;
  renderUpdate();
  const st = S.obs.state;
  if (was !== st) liveTick(); // picture appears (or goes away) right when OBS connects or drops
  const pill = $("#obs");
  pill.className = `pill tone-${TONE[st] ?? "warn"}`;
  pill.firstElementChild.textContent = has(`obs.${st}`) ? t(`obs.${st}`) : st;
  pill.title = [S.obsInfo?.url, S.obs.message].filter(Boolean).join(" · ");

  $("#detected").textContent = S.detected ? nameOf(S.detected) : t("now.noGame");
  $("#active").textContent = S.active ? nameOf(S.active) : st === "connected" ? t("now.noLayout") : "—";
  $("#scene-now").textContent = S.scene || t("now.liveScene");
  $("#mode").textContent = S.lock ? t("now.locked", { name: nameOf(S.lock) }) : t("now.auto");
  $("#mode").className = S.lock ? "tone-warn" : "";
  $("#auto").hidden = !S.lock;
  $("#new-submit").disabled = st !== "connected";
  $("#layouts-count").textContent = S.profiles.length ? t("layouts.count", { n: S.profiles.length }) : "";

  renderHelp();
  renderSteps();
  renderInfo();
  if (tab === "obs") renderConn();
  fillSelect($("#scene"), [["", t("settings.live")], ...S.scenes.map((s) => [s, s])], S.scene);
  fillSelect($("#default"), [["", t("settings.none")], ...S.profiles.map((p) => [p.id, p.name])], S.defaultProfile);
  if (document.activeElement !== $("#anim")) $("#anim").value = S.animateMs;
  $("#anim-range").value = S.animateMs;
  renderLog();

  // Rebuild the layout cards only when they changed and nobody is typing in them.
  const sig = JSON.stringify([S.profiles, S.active, S.detected, S.lock, S.defaultProfile, st, S.canvas, S.sizes]);
  if (sig !== profilesSig && !$("#profiles").contains(document.activeElement)) {
    profilesSig = sig;
    renderProfiles();
  }
}

function fillSelect(sel, options, value) {
  if (document.activeElement === sel) return;
  const sig = JSON.stringify(options);
  if (sel.dataset.sig !== sig) {
    sel.replaceChildren(...options.map(([v, text]) => h("option", { value: v, text })));
    sel.dataset.sig = sig;
  }
  sel.value = options.some(([v]) => v === value) ? value : "";
}

/** Only when OBS can't be reached: the likely cause first, then the two things that fix almost everything. */
function renderHelp() {
  const box = $("#help");
  const st = S?.obs.state;
  if (S && (st === "connected" || st === "connecting")) {
    box.hidden = true;
    return;
  }
  const info = S?.obsInfo;
  const cause = !S ? t("help.app")
    : st === "auth_failed" ? t(info?.source === "auto" ? "help.auth" : "help.authManual")
    : info?.source === "env" ? t("help.custom")
    : info?.source === "manual" ? t("help.manual", { url: info.url })
    : info && !info.detected ? t("help.notFound")
    : info?.detected?.enabled === false ? t("help.off")
    : null;
  box.hidden = false;
  box.replaceChildren(...[
    h("h2", { text: t("help.title") }),
    cause && h("p", { class: "cause", text: cause }),
    S && h("ol", { class: "fixes" }, h("li", { text: t("help.open") }), h("li", { text: t("help.enable") })),
    S && h("p", { class: "hint", text: [info?.source === "auto" ? t("help.auto") : "", info?.url ? t("help.where", { url: info.url }) : ""].join(" ").trim() }),
    S && h("div", { class: "actions" }, h("button", { type: "button", class: "small primary", onclick: () => goTo("obs"), text: t("help.settings") })),
  ].filter(Boolean));
}

// ---------------------------------------------------------------- first steps

const stepsHidden = () => store.get("kl-steps") === "hidden";
const stepsShown = () => store.get("kl-steps") === "shown";

function firstSteps() {
  return [
    { key: "obs", done: online(), label: t("steps.obs"), hint: t("steps.obsHint"), btn: t("steps.obsBtn"), go: () => goTo("obs") },
    {
      key: "default", done: Boolean(S.defaultProfile), label: t("steps.default"), hint: t("steps.defaultHint"), btn: t("steps.defaultBtn"),
      go: () => {
        goTo("layouts");
        $("#new").isDefault.checked = true;
        $("#new").exe.value = "";
        $("#new-name").focus();
      },
    },
    {
      key: "game", done: S.profiles.some((p) => p.processes.length), label: t("steps.game"), hint: t("steps.gameHint"), btn: t("steps.gameBtn"),
      go: () => {
        goTo("layouts");
        $("#new").isDefault.checked = false;
        $("#apps-refresh").click();
        $("#new-exe").focus();
      },
    },
  ];
}

function renderSteps() {
  const box = $("#steps");
  if (!S) return (box.hidden = true);
  const steps = firstSteps();
  const done = steps.filter((s) => s.done).length;
  const all = done === steps.length;
  const show = stepsShown() || (!stepsHidden() && !all);
  $("#steps-toggle").setAttribute("aria-expanded", String(show));
  box.hidden = !show;
  if (!show) return;
  const next = steps.findIndex((s) => !s.done);
  box.replaceChildren(
    h("div", { class: "steps-head" },
      h("h2", { text: all ? t("steps.allDone") : t("steps.title") }),
      h("span", { class: "steps-count", text: t("steps.count", { done, total: steps.length }) }),
      h("button", { type: "button", class: "small", onclick: () => (store.set("kl-steps", "hidden"), renderSteps()), text: t("steps.hide") }),
    ),
    h("ol", { class: "checklist" }, steps.map((s, i) => h("li", { class: s.done ? "done" : "" },
      h("span", { class: "check-mark", "aria-hidden": "true", text: s.done ? "✔" : String(i + 1) }),
      h("div", { class: "check-text" },
        h("strong", { text: s.label }),
        h("span", { class: "sr-only", text: s.done ? t("steps.srDone") : t("steps.srPending") }),
        h("small", { class: "hint", text: s.done ? t("steps.done") : s.hint }),
      ),
      s.done ? null : h("button", { type: "button", class: i === next ? "small primary" : "small", onclick: s.go, text: s.btn }),
    ))),
  );
}
$("#steps-toggle").addEventListener("click", () => {
  store.set("kl-steps", $("#steps").hidden ? "shown" : "hidden");
  renderSteps();
});

// ---------------------------------------------------------------- activity

function logText(l) {
  const key = `log.${l.key}`;
  if (!l.key || !has(key)) return l.text;
  const p = l.params ?? {};
  let text = t(key, p);
  if (l.key === "applied") {
    const extra = [has(`reason.${p.reason}`) ? t(`reason.${p.reason}`) : p.reason];
    if (p.retargeted?.length) extra.push(t("log.retargeted", { list: p.retargeted.join(", ") }));
    if (p.missing?.length) extra.push(t("log.missing", { list: p.missing.join(", ") }));
    text += ` · ${extra.join(" · ")}`;
  }
  return text;
}

function renderLog() {
  const list = $("#log");
  if (!S.log.length) return list.replaceChildren(h("li", { class: "hint", text: t("log.empty") }));
  list.replaceChildren(...S.log.map((l) => h("li", { class: l.key === "applyFailed" ? "tone-bad" : "" },
    h("time", { datetime: l.at, text: new Date(l.at).toLocaleTimeString(getLanguage(), { hour: "2-digit", minute: "2-digit", second: "2-digit" }) }),
    h("span", { text: logText(l) }))));
}

// ---------------------------------------------------------------- layout cards

function renderProfiles() {
  const box = $("#profiles");
  if (!S.profiles.length) {
    // The one walkthrough on the page: three steps, gone once the first layout exists.
    box.replaceChildren(h("div", { class: "empty" },
      h("strong", { text: t("empty.title") }),
      h("ol", {}, ["empty.1", "empty.2", "empty.3"].map((k) => h("li", { text: t(k) })))));
    return;
  }
  box.replaceChildren(...S.profiles.map((p) => profileCard(p, online())));
}

function profileCard(p, connected) {
  const badges = [
    p.id === S.active && h("span", { class: "badge tone-ok", text: t("badge.onScreen") }),
    p.id === S.detected && h("span", { class: "badge tone-warn", text: t("badge.running") }),
    p.id === S.lock && h("span", { class: "badge tone-warn", text: t("badge.locked") }),
    p.id === S.defaultProfile && h("span", { class: "badge", text: t("badge.default") }),
  ];
  const msg = h("p", { class: "msg", role: "status", hidden: true });
  const run = (fn) => async () => {
    try {
      await fn();
    } catch (e) {
      flash(msg, e.message, "bad");
    }
  };
  const names = Object.keys(p.items);
  const included = names.filter((n) => p.items[n].include !== false).length;
  const nameIn = h("input", { class: "name", value: p.name, maxlength: 60, "aria-label": t("card.name") });
  const exeIn = h("input", { class: "mono-input", value: p.processes.join(", "), placeholder: t("card.gamesPh"), "aria-label": t("card.games") });
  const saveMeta = run(async () => {
    await api(`/api/profiles/${p.id}`, "PUT", { name: nameIn.value, processes: exeIn.value.split(",") });
    toast(t("card.saved"));
  });
  nameIn.addEventListener("change", saveMeta);
  exeIn.addEventListener("change", saveMeta);
  const thumb = layoutThumb(p);
  const highlight = (name, on) => thumb.svg?.querySelectorAll(`[data-name="${CSS.escape(name)}"]`).forEach((el) => el.classList.toggle("hot", on));

  return h("article", { class: `profile${p.id === S.active ? " is-active" : ""}` },
    h("div", { class: "profile-head" }, nameIn, h("span", { class: "badges" }, badges)),
    h("div", { class: "profile-main" },
      thumb.el,
      h("div", { class: "profile-info" },
        h("label", { class: "games" }, h("span", { text: t("card.games") }), exeIn),
        p.processes.length ? null : h("small", { class: "hint", text: t("card.noGames") }),
        captureList(p),
      ),
    ),
    h("div", { class: "actions" },
      h("button", { type: "button", disabled: !connected, onclick: run(async () => {
        await api(`/api/profiles/${p.id}`, "POST");
        toast(t("toast.applied", { name: p.name }));
      }), text: t("btn.apply"), title: t("btn.applyTitle") }),
      p.id === S.lock
        ? h("button", { type: "button", class: "active", onclick: run(() => api("/api/lock", "POST", { id: null })), text: t("btn.unlock") })
        : h("button", { type: "button", onclick: run(() => api("/api/lock", "POST", { id: p.id })), text: t("btn.lock"), title: t("btn.lockTitle") }),
      h("button", { type: "button", disabled: !connected, title: t("btn.updateTitle"), onclick: run(async () => {
        if (!confirm(t("confirm.update", { name: p.name }))) return;
        await api(`/api/profiles/${p.id}/capture`, "POST");
        toast(t("toast.updated", { name: p.name }));
      }), text: t("btn.update") }),
      h("details", { class: "k-menu more" },
        h("summary", { "aria-label": t("btn.more"), title: t("btn.more") }, "⋯"),
        h("div", { class: "k-menu-panel more-panel" },
          p.id === S.defaultProfile ? null : h("button", { type: "button", onclick: run(async () => {
            await api("/api/settings", "PUT", { defaultProfile: p.id });
            toast(t("toast.default", { name: p.name }));
          }), text: t("btn.makeDefault") }),
          h("button", { type: "button", onclick: run(async () => {
            await api(`/api/profiles/${p.id}/duplicate`, "POST", { name: t("card.copyName", { name: p.name }).slice(0, 60) });
            toast(t("toast.duplicated", { name: p.name }));
          }), text: t("btn.duplicate"), title: t("btn.duplicateTitle") }),
          h("button", { type: "button", class: "danger", onclick: run(async () => {
            if (confirm(t("confirm.delete", { name: p.name }))) await api(`/api/profiles/${p.id}`, "DELETE");
          }), text: t("btn.delete") }),
        ),
      ),
    ),
    h("details", { class: "sources-box" },
      h("summary", { text: t("sources.summary", { n: included, total: names.length }) }),
      h("p", { class: "hint", text: t("sources.hint") }),
      h("div", { class: "sources" }, names.map((n) => {
        const it = p.items[n];
        const box = h("input", { type: "checkbox", checked: it.include !== false });
        box.addEventListener("change", run(() => api(`/api/profiles/${p.id}`, "PUT", { include: { [n]: box.checked } })));
        return h("label", { class: "check", onmouseenter: () => highlight(n, true), onmouseleave: () => highlight(n, false) },
          box, h("span", { text: n }), it.visible ? null : h("small", { class: "hint", text: t("sources.hidden") }));
      })),
    ),
    msg,
  );
}

// ---------------------------------------------------------------- layout thumbnail

const ALIGN_LEFT = 1, ALIGN_RIGHT = 2, ALIGN_TOP = 4, ALIGN_BOTTOM = 8;
const along = (a, lo, hi, size) => (a & lo ? 0 : a & hi ? size : size / 2);

/** Where a source sits on the canvas, from its saved transform (and its size when known). Mirrors OBS's own
 *  maths for crop, scale, bounds and alignment closely enough for a thumbnail; rotation is applied by the SVG. */
export function boxOf(tr, size) {
  const cw = size ? size.w - (tr.cropLeft || 0) - (tr.cropRight || 0) : 0;
  const ch = size ? size.h - (tr.cropTop || 0) - (tr.cropBottom || 0) : 0;
  const bounds = tr.boundsType && tr.boundsType !== "OBS_BOUNDS_NONE";
  let w, h;
  let ox = 0, oy = 0; // content offset inside the bounds box
  if (bounds) {
    w = tr.boundsWidth;
    h = tr.boundsHeight;
    if (cw > 0 && ch > 0 && tr.boundsType !== "OBS_BOUNDS_STRETCH") {
      const k = {
        OBS_BOUNDS_SCALE_INNER: Math.min(w / cw, h / ch),
        OBS_BOUNDS_SCALE_OUTER: Math.max(w / cw, h / ch),
        OBS_BOUNDS_SCALE_TO_WIDTH: w / cw,
        OBS_BOUNDS_SCALE_TO_HEIGHT: h / ch,
        OBS_BOUNDS_MAX_ONLY: Math.min(1, w / cw, h / ch),
      }[tr.boundsType];
      if (k > 0 && tr.boundsType !== "OBS_BOUNDS_SCALE_OUTER") {
        const ba = tr.boundsAlignment ?? 0;
        ox = along(ba, ALIGN_LEFT, ALIGN_RIGHT, w - cw * k);
        oy = along(ba, ALIGN_TOP, ALIGN_BOTTOM, h - ch * k);
        [w, h] = [cw * k, ch * k];
      }
    }
  } else {
    if (!(cw > 0 && ch > 0)) return null;
    w = cw * Math.abs(tr.scaleX ?? 1);
    h = ch * Math.abs(tr.scaleY ?? 1);
  }
  if (!(w > 0 && h > 0)) return null;
  const a = tr.alignment ?? 5;
  const bw = bounds ? tr.boundsWidth : w;
  const bh = bounds ? tr.boundsHeight : h;
  return {
    x: tr.positionX - along(a, ALIGN_LEFT, ALIGN_RIGHT, bw) + ox,
    y: tr.positionY - along(a, ALIGN_TOP, ALIGN_BOTTOM, bh) + oy,
    w, h, rot: tr.rotation || 0, px: tr.positionX, py: tr.positionY,
  };
}

function layoutThumb(p) {
  const canvas = p.canvas ?? S.canvas ?? { width: 1920, height: 1080 };
  const W = canvas.width;
  const H = canvas.height;
  const fs = Math.round(W / 26);
  const full = [];
  const shapes = [];
  let unknown = 0;
  for (const [name, it] of Object.entries(p.items)) {
    if (it.include === false || !it.visible || it.capture?.kind === "wasapi_process_output_capture") continue;
    const size = it.size ?? S.sizes?.[name];
    const bounded = it.transform.boundsType && it.transform.boundsType !== "OBS_BOUNDS_NONE";
    if (size && !(size.w > 0 && size.h > 0) && !bounded) continue; // no picture (audio, empty capture)
    const b = boxOf(it.transform, size);
    if (!b) {
      unknown++;
      continue;
    }
    const cls = it.capture && it.capture.kind !== "wasapi_process_output_capture" ? "src cap" : "src";
    const covers = !b.rot && b.x <= W * 0.02 && b.y <= H * 0.02 && b.x + b.w >= W * 0.98 && b.y + b.h >= H * 0.98;
    if (covers) full.push(name);
    const rect = h("svg:rect", { x: b.x, y: b.y, width: b.w, height: b.h, class: covers ? `${cls} full` : cls, "data-name": name, transform: b.rot ? `rotate(${b.rot} ${b.px} ${b.py})` : null },
      h("svg:title", { text: name }));
    shapes.push(rect);
    // Name inside the box when there's room; full-screen sources are listed under the picture instead.
    if (!covers && b.w > W * 0.1 && b.h > fs * 1.4) {
      const max = Math.max(3, Math.floor((b.w - fs * 0.6) / (fs * 0.58)));
      shapes.push(h("svg:text", { x: b.x + fs * 0.35, y: b.y + fs * 1.05, "font-size": fs, class: "label", "data-name": name, transform: b.rot ? `rotate(${b.rot} ${b.px} ${b.py})` : null },
        name.length > max ? `${name.slice(0, max - 1)}…` : name));
    }
  }
  const svg = h("svg:svg", { viewBox: `0 0 ${W} ${H}`, class: "thumb-svg", role: "img", "aria-label": t("thumb.aria", { name: p.name }) },
    h("svg:rect", { x: 0, y: 0, width: W, height: H, class: "canvas" }),
    shapes);
  const notes = [full.length ? t("thumb.full", { list: full.join(", ") }) : null, unknown ? t("thumb.unknown", { n: unknown }) : null].filter(Boolean);
  const el = h("figure", { class: "thumb" }, svg, h("figcaption", { class: "hint", text: notes.join(" · ") || `${W}×${H}` }));
  return { el, svg };
}

/** OBS window string "Title:Class:game.exe" (":" and "#" escaped as #3A / #22) -> program name. */
const windowExe = (s) => (String(s || "").split(":")[2] || "").replace(/#3A/g, ":").replace(/#22/g, "#").toLowerCase();

/** "Game Capture → pathofexilesteam.exe" for each capture source this layout shows, with a warning when a
 *  game/window capture points at a program that is not one of the layout's games. */
function captureList(p) {
  const rows = Object.entries(p.items).filter(([, it]) => it.capture && it.visible && it.include !== false);
  if (!rows.length) return null;
  return h("ul", { class: "captures" }, rows.map(([name, it]) => {
    const s = it.capture.settings;
    const kind = t(it.capture.kind === "window_capture" ? "cap.window" : it.capture.kind === "game_capture" ? "cap.game" : "cap.audio");
    const mode = it.capture.kind === "game_capture" ? s.capture_mode || "any_fullscreen" : "window";
    const anyFull = mode === "any_fullscreen";
    const exe = windowExe(s.window);
    const target = anyFull ? t("cap.any") : mode === "hotkey" ? t("cap.hotkey") : exe || t("cap.none");
    // Only the picture should follow the game. App audio usually captures other apps on purpose (Discord, music).
    const video = it.capture.kind === "game_capture" || it.capture.kind === "window_capture";
    const off = video && p.processes.length > 0 && mode !== "hotkey" && (anyFull || !p.processes.includes(exe));
    return h("li", { class: `${video ? "video" : "audio"}${off ? " warn" : ""}` },
      h("span", { class: "src", text: name }),
      h("span", { class: "arrow", "aria-hidden": "true", text: "→" }),
      h("strong", { text: target }),
      h("small", { text: kind }),
      off && h("em", { text: t(anyFull ? "cap.anyWarn" : "cap.offWarn") }));
  }));
}

// ---------------------------------------------------------------- live picture

const livePaused = () => store.get("kl-live") === "paused";
function renderLiveToggle() {
  const b = $("#live-toggle");
  const paused = livePaused();
  b.textContent = t(paused ? "live.resume" : "live.pause");
  b.setAttribute("aria-pressed", String(!paused));
}
$("#live-toggle").addEventListener("click", () => {
  store.set("kl-live", livePaused() ? "on" : "paused");
  renderLiveToggle();
  liveTick();
});

let liveBusy = false;
async function liveTick() {
  clearTimeout(liveTick.timer);
  const img = $("#shot");
  const empty = $("#screen-empty");
  const visible = tab === "layouts" && document.visibilityState === "visible";
  if (visible && !liveBusy) {
    if (!online()) {
      img.hidden = true;
      empty.hidden = false;
      empty.textContent = t("live.off");
      $("#screen-caption").textContent = "";
    } else if (livePaused()) {
      empty.hidden = !img.hidden;
      empty.textContent = t("live.paused");
    } else {
      liveBusy = true;
      try {
        const shot = await (await fetch(`/api/screenshot?w=${Math.min(960, Math.round($("#screen").clientWidth * devicePixelRatio) || 640)}`)).json();
        if (shot.image) {
          img.src = shot.image;
          img.alt = t("live.alt", { scene: shot.scene });
          img.hidden = false;
          empty.hidden = true;
          const c = S?.canvas;
          $("#screen-caption").textContent = [shot.scene, c ? `${c.width}×${c.height}` : ""].filter(Boolean).join(" · ");
        }
      } catch {
        // next tick tries again
      } finally {
        liveBusy = false;
      }
    }
  }
  liveTick.timer = setTimeout(liveTick, 2000);
}
document.addEventListener("visibilitychange", liveTick);

// ---------------------------------------------------------------- new layout

$("#new").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = e.target;
  const msg = $("#new-msg");
  try {
    await api("/api/profiles", "POST", { name: f.name.value, processes: f.exe.value ? f.exe.value.split(",") : [], isDefault: f.isDefault.checked });
    flash(msg, t("new.saved", { name: f.name.value }));
    toast(t("new.saved", { name: f.name.value }));
    f.reset();
    $("#apps").hidden = true;
  } catch (err) {
    flash(msg, err.message, "bad");
  }
});

// Programs with an open window, as a short pick list (the game has to be running).
$("#apps-refresh").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  const list = $("#apps");
  btn.disabled = true;
  btn.textContent = t("new.looking");
  try {
    const apps = await api("/api/apps");
    list.hidden = false;
    list.replaceChildren(...(apps.length ? apps.map((a) => h("li", {},
      h("button", { type: "button", class: "app", title: a.title, onclick: () => {
        $("#new").exe.value = a.exe;
        if (!$("#new").name.value) $("#new").name.value = a.title.split(/ [-–|] /)[0].slice(0, 60);
        list.hidden = true;
        $("#new").exe.focus();
      } }, h("span", { text: a.title || a.exe }), h("code", { text: a.exe })))) : [h("li", { class: "hint", text: t("new.noApps") })]));
  } catch (err) {
    list.hidden = false;
    list.replaceChildren(h("li", { class: "hint tone-bad", text: err.message }));
  } finally {
    btn.disabled = false;
    btn.textContent = t("new.pick");
  }
});

// ---------------------------------------------------------------- OBS tab: connection

let connFilled = false; // the form shows the saved values until the user starts editing
const connForm = $("#conn");
const connMode = () => connForm.mode.value || "auto";

function renderConn() {
  const info = S?.obsInfo;
  if (!info) return;
  const env = info.source === "env";
  $("#conn-env").hidden = !env;
  if (env) $("#conn-env").textContent = t("conn.env", { url: info.url });
  connForm.querySelectorAll("input, button[type=submit]").forEach((el) => (el.disabled = env));

  if (!connFilled && !connForm.contains(document.activeElement)) {
    connFilled = true;
    connForm.mode.value = info.mode;
    // First switch to manual: start from what OBS on this PC uses, which is usually what people want to tweak.
    $("#conn-host").value = info.host;
    $("#conn-port").value = info.mode === "manual" ? info.port : info.detected?.port ?? info.port;
    $("#conn-pass").value = "";
    $("#conn-clear").checked = false;
    clearFieldErrors();
  }
  const manual = connMode() === "manual";
  $("#conn-manual").hidden = !manual;
  $("#conn-auto").hidden = manual;
  connForm.querySelectorAll(".mode-card").forEach((c) => c.classList.toggle("on", c.querySelector("input").checked));

  const badge = $("#conn-pass-badge");
  badge.className = `badge${info.hasPassword ? " tone-ok" : ""}`;
  badge.textContent = info.hasPassword ? t("conn.passSaved") : t("conn.passNone");
  $("#conn-pass").placeholder = info.hasPassword ? t("conn.passKeep") : t("conn.passPh");
  $("#conn-clear-row").hidden = !info.hasPassword;

  const d = info.detected;
  $("#conn-auto").replaceChildren(
    d
      ? h("div", { class: `detected-box ${d.enabled ? "ok" : "bad"}` },
          h("strong", { text: d.enabled ? t("conn.found") : t("conn.foundOff") }),
          h("dl", {},
            h("dt", { text: t("conn.port") }), h("dd", { class: "mono", text: String(d.port) }),
            h("dt", { text: t("conn.password") }), h("dd", { text: d.authRequired ? t("conn.detPass") : t("conn.detNoPass") })),
          d.enabled ? null : h("p", { class: "hint", text: t("help.enable") }))
      : h("div", { class: "detected-box bad" },
          h("strong", { text: t("conn.notFound") }),
          h("p", { class: "hint", text: t("conn.notFoundHint") })),
  );
}

function clearFieldErrors() {
  connForm.querySelectorAll(".field.has-error").forEach((f) => f.classList.remove("has-error"));
  connForm.querySelectorAll(".field-error").forEach((e) => e.remove());
}

connForm.addEventListener("change", (e) => {
  if (e.target.name === "mode") renderConn();
});
$("#conn-pass-show").addEventListener("click", (e) => {
  const input = $("#conn-pass");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  e.currentTarget.textContent = t(show ? "conn.hide" : "conn.show");
  e.currentTarget.setAttribute("aria-pressed", String(show));
});

/** After a save or reconnect: what happened, in words. */
function connOutcome() {
  const st = S.obs.state;
  if (st === "connected") return [t("conn.ok", { url: S.obsInfo.url }), "ok"];
  if (st === "auth_failed") return [t("conn.badPass"), "bad"];
  if (st === "connecting") return [t("conn.trying"), "warn"];
  return [t("conn.noAnswer", { url: S.obsInfo.url }), "bad"];
}

connForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  clearFieldErrors();
  const body = { mode: connMode() };
  if (body.mode === "manual") {
    body.host = $("#conn-host").value.trim();
    body.port = $("#conn-port").value === "" ? 4455 : Number($("#conn-port").value);
    if ($("#conn-pass").value) body.password = $("#conn-pass").value;
    else if ($("#conn-clear").checked) body.clearPassword = true;
  }
  const btn = $("#conn-save");
  const msg = $("#conn-msg");
  btn.disabled = true;
  flash(msg, t("conn.trying"), "warn");
  try {
    await api("/api/obs", "PUT", body);
    connFilled = false;
    renderConn();
    const [text, tone] = connOutcome();
    flash(msg, text, tone);
    toast(text, tone);
  } catch (err) {
    const fields = err.data?.fields ?? [];
    for (const f of fields) {
      const field = { host: "#conn-host", port: "#conn-port", password: "#conn-pass" }[f.field];
      const wrap = field && $(field).closest(".field");
      if (!wrap) continue;
      wrap.classList.add("has-error");
      wrap.append(h("small", { class: "hint tone-bad field-error", text: t(`conn.err.${f.field}`) }));
    }
    flash(msg, fields.length ? t("conn.fix") : err.message, "bad");
  } finally {
    btn.disabled = S?.obsInfo?.source === "env";
  }
});

$("#conn-retry").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  flash($("#conn-msg"), t("conn.trying"), "warn");
  try {
    await api("/api/obs/reconnect", "POST", {});
    const [text, tone] = connOutcome();
    flash($("#conn-msg"), text, tone);
  } catch (err) {
    flash($("#conn-msg"), err.message, "bad");
  } finally {
    btn.disabled = false;
  }
});

// ---------------------------------------------------------------- OBS tab: status + behaviour

function renderInfo() {
  const info = S.obsInfo;
  const st = S.obs.state;
  const row = (k, v, cls) => [h("dt", { text: t(k) }), h("dd", { class: cls, text: v ?? "—" })];
  $("#info").replaceChildren(
    ...row("info.state", has(`obs.${st}`) ? t(`obs.${st}`) : st, `tone-${TONE[st] ?? "warn"}`),
    ...row("info.address", info?.url, "mono"),
    ...row("info.source", info ? t(`info.src.${info.source}`) : null),
    ...row("info.version", S.obsVersion?.obs ? `OBS ${S.obsVersion.obs} · WebSocket ${S.obsVersion.websocket ?? "?"}` : null),
    ...row("info.canvas", S.canvas ? `${S.canvas.width}×${S.canvas.height}` : null, "mono"),
    ...row("info.scenes", S.scenes.length ? String(S.scenes.length) : null),
    ...(S.obs.message && st !== "connected" ? row("info.message", S.obs.message) : []),
  );
}

const saveSettings = async (body) => {
  try {
    await api("/api/settings", "PUT", body);
    toast(t("settings.saved"));
  } catch (err) {
    toast(err.message, "bad");
  }
};
$("#scene").addEventListener("change", (e) => saveSettings({ scene: e.target.value }));
$("#default").addEventListener("change", (e) => saveSettings({ defaultProfile: e.target.value }));
$("#anim").addEventListener("change", (e) => saveSettings({ animateMs: Math.max(0, Math.min(3000, Math.round(Number(e.target.value) || 0))) }));
$("#anim-range").addEventListener("input", (e) => ($("#anim").value = e.target.value));
$("#anim-range").addEventListener("change", (e) => saveSettings({ animateMs: Number(e.target.value) }));
$("#auto").addEventListener("click", () => api("/api/lock", "POST", { id: null }));

// ---------------------------------------------------------------- polling

/** Banner when the installed app has an update. Dismissing hides it until there is an even newer one. */
function renderUpdate(force = false) {
  const box = $("#update");
  const u = S?.update;
  const sig = JSON.stringify(u ?? null) + getLanguage();
  if (sig === updateSig && !force) return;
  updateSig = sig;
  if (u?.state === "installing") {
    box.hidden = false;
    return box.replaceChildren(
      h("strong", { text: t("update.installing", { version: u.latest?.version ?? "" }) }),
      h("span", { text: t("update.restarting") }),
    );
  }
  const key = u?.state === "available" ? `${u.url}|${u.latest?.version}` : "";
  box.hidden = !key || store.get(UPDATE_DISMISS_KEY) === key;
  if (box.hidden) return box.replaceChildren();
  box.replaceChildren(
    h("strong", { text: t("update.title") }),
    h("span", { text: t("update.version", { version: u.latest.version }) }),
    h("span", { text: t(u.error ? "update.failed" : u.install ? "update.how" : "update.howDownload") }),
    h("a", { href: u.url, target: "_blank", rel: "noopener noreferrer", text: t(u.install && !u.error ? "update.changes" : "update.download") }),
    u.install
      ? h("button", { type: "button", class: "small primary", text: t(u.error ? "update.retry" : "update.install"), onclick: async (e) => {
          const button = e.currentTarget;
          button.disabled = true;
          try {
            await api("/api/update/install", "POST", {});
          } catch {
            button.disabled = false;
          }
        } })
      : null,
    h("button", { type: "button", class: "small", text: t("update.dismiss"), onclick: () => {
      store.set(UPDATE_DISMISS_KEY, key);
      renderUpdate(true);
    } }),
  );
}

async function poll() {
  try {
    apply(await (await fetch("/api/state")).json());
  } catch {
    const pill = $("#obs");
    pill.className = "pill tone-bad";
    pill.firstElementChild.textContent = t("obs.app");
    S = null;
    renderHelp();
    renderSteps();
  }
  setTimeout(poll, 1500);
}

applyLanguage(initialLanguage(), false);
setTab(location.hash.slice(1));
poll();
