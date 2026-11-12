"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const lang = (navigator.language || "en").toLowerCase().startsWith("cs") ? "cs" : "en";
const TEXT = window.LAD_TEXTS[lang];
document.documentElement.lang = lang;

function t(key, vars = {}) {
  let s = TEXT[key] ?? window.LAD_TEXTS.en[key] ?? key;
  for (const [k, v] of Object.entries(vars)) s = s.split(`{${k}}`).join(String(v));
  return s;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) node.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
}

const timeFmt = new Intl.DateTimeFormat(lang, { hour: "2-digit", minute: "2-digit" });
const stampFmt = new Intl.DateTimeFormat(lang, { day: "numeric", month: "numeric", hour: "2-digit", minute: "2-digit" });

let view = null;
let pairing = false;
// Inline confirmations: native confirm() is not reliable in every webview.
let confirmRemove = null;
let confirmQuit = false;

// Static texts and "?" toggles.
for (const node of document.querySelectorAll("[data-i18n]")) node.textContent = t(node.dataset.i18n);
for (const node of document.querySelectorAll("[data-i18n-placeholder]")) node.placeholder = t(node.dataset.i18nPlaceholder);
for (const node of document.querySelectorAll("[data-i18n-title]")) {
  node.title = t(node.dataset.i18nTitle);
  node.setAttribute("aria-label", t(node.dataset.i18nTitle));
}
for (const btn of document.querySelectorAll(".help-toggle")) {
  btn.addEventListener("click", () => {
    const help = document.getElementById(btn.dataset.help);
    help.hidden = !help.hidden;
    btn.setAttribute("aria-expanded", String(!help.hidden));
  });
}

function statusLine(env) {
  const s = env.status;
  let dot = "";
  let text = t("state_off");
  let detail = null;
  switch (s.state) {
    case "connecting":
      dot = "connecting"; text = t("state_connecting"); break;
    case "connected":
      dot = "on"; text = t("state_connected"); break;
    case "working": {
      dot = "busy"; text = t("state_working");
      const app = s.app ? t("working_in", { app: s.app }) : "";
      detail = t("working_detail", { tool: s.tool || "", app, since: timeFmt.format(new Date(s.sinceMs)) });
      break;
    }
    case "waiting": {
      dot = s.reason === "stopped" ? "connecting" : "problem";
      text = t(`reason_${s.reason || "error"}`);
      if (s.retryAtMs) {
        const left = Math.max(0, Math.ceil((s.retryAtMs - Date.now()) / 1000));
        if (left > 0) detail = t("retry_in", { s: left });
      }
      break;
    }
  }
  return el("div", { class: "status" },
    el("span", { class: `dot ${dot}` }),
    el("div", {}, text, detail ? el("div", { class: "note" }, detail) : null));
}

function renderEnvironments() {
  const list = document.getElementById("environments");
  list.replaceChildren();
  document.getElementById("no-env").hidden = view.environments.length > 0;
  for (const env of view.environments) {
    const toggle = el("input", {
      type: "checkbox", role: "switch", "aria-label": `${t("switchLabel")}: ${env.label}`,
      onchange: async (e) => {
        e.target.disabled = true;
        try { await invoke("set_enabled", { id: env.id, enabled: e.target.checked }); } finally { refresh(); }
      },
    });
    toggle.checked = env.enabled;
    const active = env.status.state === "connected" || env.status.state === "working";
    list.append(el("li", { class: "card" },
      el("div", { class: "card-head" },
        el("div", { class: "card-title" }, el("strong", {}, env.label), el("small", {}, env.launchpad.replace(/^https?:\/\//, ""))),
        el("label", { class: "switch" }, toggle, el("span"))),
      statusLine(env),
      el("div", { class: "actions" },
        active ? el("button", { class: "stop", onclick: () => invoke("stop", { id: env.id }).then(refresh) }, t("stop")) : null,
        el("button", { class: "link", onclick: () => invoke("open_launchpad", { id: env.id }) }, t("manage")),
        el("button", { class: "link danger", onclick: () => { confirmRemove = env.id; renderEnvironments(); } }, t("remove"))),
      confirmRemove === env.id
        ? el("div", { class: "confirm" },
          el("p", { class: "note" }, t("removeConfirm", { name: env.label })),
          el("div", { class: "actions" },
            el("button", {
              class: "stop danger",
              onclick: async () => { confirmRemove = null; await invoke("remove_environment", { id: env.id }); refresh(); },
            }, t("remove")),
            el("button", { class: "link", onclick: () => { confirmRemove = null; renderEnvironments(); } }, t("cancel"))))
        : null));
  }
}

function renderPermissions() {
  const box = document.getElementById("permissions");
  const p = view.permissions;
  box.hidden = !p.needed || (p.allGranted && view.onboarded);
  for (const row of box.querySelectorAll(".perm-row")) {
    const kind = row.dataset.kind;
    const granted = kind === "accessibility" ? p.accessibility : p.screenRecording;
    const cell = row.querySelector(".perm-state");
    cell.replaceChildren(granted
      ? el("span", { class: "ok" }, t("permGranted"))
      : el("button", { class: "primary", onclick: () => invoke("request_permission", { kind }) }, t("permAllow")));
  }
  if (p.allGranted && !view.onboarded && view.environments.length > 0) invoke("finish_onboarding");
}

async function renderHistory() {
  const box = document.getElementById("history-box");
  if (!box.open) return;
  const entries = await invoke("history", { limit: 100 });
  const labels = Object.fromEntries(view.environments.map((e) => [e.id, e.label]));
  const list = document.getElementById("history");
  list.replaceChildren();
  if (entries.length === 0) list.append(el("li", {}, el("span"), t("historyEmpty")));
  for (const e of entries) {
    let text;
    if (e.event === "tool") text = t(e.ok ? "ev_tool_ok" : "ev_tool_err", { tool: e.tool, ms: e.ms ?? 0 });
    else text = t(`ev_${e.event}`);
    if (e.event === "disconnected" && e.reason) text += ` · ${t(`reason_${e.reason}`)}`;
    const who = labels[e.env] || t("removedEnv");
    list.append(el("li", {},
      el("time", { datetime: e.ts }, stampFmt.format(new Date(e.ts))),
      el("span", {}, `${who}: ${text}`)));
  }
}

function renderUpdate() {
  const u = view.update;
  const select = document.getElementById("channel");
  if (document.activeElement !== select) select.value = u.channel;
  const box = document.getElementById("update");
  const button = document.getElementById("update-button");
  let text = "";
  if (u.installing) text = t("updateInstalling");
  else if (u.available) text = t("updateAvailable", { version: u.available });
  if (!u.installing && u.available && u.error) text += " " + t("updateFailed", { message: u.error });
  document.getElementById("update-text").textContent = text;
  button.hidden = !u.available || u.installing;
  box.hidden = !text;
}

function renderPairing() {
  document.getElementById("pairing").hidden = !pairing;
  document.querySelector("#add-form button").disabled = pairing;
}

async function refresh() {
  view = await invoke("state");
  document.getElementById("version").textContent = `v${view.version} · ${t(view.update.channel === "pilot" ? "channelPilot" : "channelStable")}`;
  document.getElementById("autostart").checked = view.autostart;
  document.getElementById("computer-name").textContent = t("addAs", { name: view.computerName });
  renderEnvironments();
  renderPermissions();
  renderPairing();
  renderUpdate();
  renderHistory();
}

function showError(err) {
  const box = document.getElementById("add-error");
  const code = err && err.code ? err.code : "other";
  const msg = t(`error_${code}`, { message: (err && err.message) || String(err) });
  box.textContent = msg;
  box.hidden = !msg;
}

document.getElementById("add-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = document.getElementById("launchpad");
  document.getElementById("add-error").hidden = true;
  try {
    const started = await invoke("begin_pairing", { launchpad: input.value });
    document.getElementById("fingerprint").textContent = started.fingerprint;
    pairing = true;
    renderPairing();
    await invoke("wait_pairing");
    input.value = "";
  } catch (err) {
    showError(err);
  } finally {
    pairing = false;
    refresh();
  }
});

document.getElementById("update-button").addEventListener("click", async (e) => {
  e.target.disabled = true;
  try { await invoke("install_update"); } catch (err) { /* shown from state */ } finally { e.target.disabled = false; refresh(); }
});
document.getElementById("channel").addEventListener("change", async (e) => {
  e.target.disabled = true;
  try { await invoke("set_update_channel", { channel: e.target.value }); } finally { e.target.disabled = false; refresh(); }
});
document.getElementById("pairing-cancel").addEventListener("click", () => invoke("cancel_pairing"));
document.getElementById("perm-recheck").addEventListener("click", refresh);
document.getElementById("history-box").addEventListener("toggle", renderHistory);
document.getElementById("autostart").addEventListener("change", async (e) => {
  try { await invoke("set_autostart", { enabled: e.target.checked }); } finally { refresh(); }
});
function renderQuit() {
  document.getElementById("quit-confirm").hidden = !confirmQuit;
}
document.getElementById("quit").addEventListener("click", () => { confirmQuit = true; renderQuit(); });
document.getElementById("quit-yes").addEventListener("click", () => invoke("quit"));
document.getElementById("quit-no").addEventListener("click", () => { confirmQuit = false; renderQuit(); });

listen("changed", () => refresh());
window.addEventListener("focus", refresh);
// Countdown and "since" times; cheap, the panel is small.
setInterval(() => {
  if (!view || document.visibilityState !== "visible") return;
  if (view.environments.some((e) => e.status.state === "waiting" || e.status.state === "working")) renderEnvironments();
}, 1000);
refresh();
