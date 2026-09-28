import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { esc, toast, openConfirmModal, openInputModal, dismissSplash } from "./ui.js";
import { ic } from "./icons.js";
import { init, t, lang } from "./i18n.js";
import lobsterLogo from "./assets/lobster-logo.png";

const $app = document.getElementById("app");
let state = null;
let busy = false;

async function guard(fn) {
  if (busy) return;
  busy = true;
  setButtonsDisabled(true);
  try {
    await fn();
  } catch (e) {
    toast(stripErr(e), "err");
  } finally {
    busy = false;
    setButtonsDisabled(false);
  }
}

function stripErr(e) {
  return typeof e === "string" ? e : String(e);
}

async function refresh() {
  state = await invoke("get_state");
  if (state?.language) init(state.language);
  render();
}

function setButtonsDisabled(v) {
  $app.querySelectorAll("button").forEach((b) => (b.disabled = v));
}

function daysText(days) {
  if (days == null || !isFinite(days)) return null;
  const d = Math.round(days * 10) / 10;
  return lang() === "zh" ? `${d} 天` : `${d} d`;
}

function healthInfo(acc) {
  const days = acc.token_days_left;
  let key = "healthHintUnknown";
  if (days != null && days <= 0) key = "healthHintExpired";
  else if (days != null && days <= 3) key = "healthHintWarn";
  else if (days != null) key = "healthHintOk";
  return t(key, { days: daysText(days) ?? "?" });
}

function fmtSize(bytes) {
  if (bytes == null) return "—";
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function checkinChip(acc) {
  const c = acc.last_checkin;
  const today = new Date();
  const p = (n) => String(n).padStart(2, "0");
  const todayStr = `${today.getFullYear()}-${p(today.getMonth() + 1)}-${p(today.getDate())}`;
  if (!c) return `<span class="chip-checkin none" title="${t("checkinNone")}">${t("checkinNone")}</span>`;
  const isToday = c.date === todayStr && (c.status === "ok" || c.status === "done" || c.status === "no_activity");
  const cls = isToday ? "ok" : "miss";
  const label = isToday ? t("checkinOk") : t("checkinMiss");
  return `<span class="chip-checkin ${cls}" title="${esc(c.msg || "")}">${label}</span>`;
}

function proxyCard() {
  const p = state.proxy || {};
  const running = !!p.running;
  const upstreamOk = p.upstream_alive === true;
  const port = p.port || 19260;
  const upstream = p.upstream ? esc(p.upstream) : "—";
  return `
  <div class="card proxy-card ${running ? "active" : ""}">
    <div class="card-top">
      <span class="card-name">${ic("plug", 15)} ${t("proxyTitle")}</span>
      <span class="health ${running ? "ok" : ""}"><span class="dot"></span>${running ? t("chipProxyOn") : t("chipProxyOff")}</span>
    </div>
    <div class="card-sub">${t("proxyDesc")}</div>
    <div class="proxy-urls">
      <span class="proxy-url"><b>${t("proxyOpenaiUrl")}</b><code>http://127.0.0.1:${port}/v1/chat/completions</code></span>
      <span class="proxy-url"><b>${t("proxyAnthropicUrl")}</b><code>http://127.0.0.1:${port}/v1/messages</code></span>
      <span class="proxy-url"><b>${t("proxyUpstream")}</b><code>${upstream}</code> ${running ? (upstreamOk ? `<span class="chip on"><span class="dot"></span>${t("chipUpstreamOk")}</span>` : `<span class="chip warn"><span class="dot"></span>${t("chipUpstreamDown")}</span>`) : ""}</span>
    </div>
    <div class="card-actions">
      ${running
        ? `<button class="mini-btn" data-act="proxyStop">${ic("stop", 12)}${t("proxyStop")}</button>`
        : `<button class="mini-btn primary" data-act="proxyStart">${ic("play", 12)}${t("proxyStart")}</button>`}
      <button class="mini-btn" data-act="proxyLog">${ic("doc", 12)}${t("proxyLog")}</button>
      <span class="spacer"></span>
      <button class="mini-btn" data-act="register">${ic("switch", 12)}${t("registerBtn")}</button>
    </div>
  </div>`;
}

function render() {
  if (!state) return;
  const chips = [];
  chips.push(`<span class="chip ${state.lobster_running ? "on" : ""}"><span class="dot"></span>${t(state.lobster_running ? "chipLobsterOn" : "chipLobsterOff")}</span>`);
  if (state.live_logged_in) {
    const known = state.active_account_id != null;
    chips.push(`<span class="chip ${known ? "on" : "warn"}"><span class="dot"></span>${t("chipLoggedIn")}${state.live_name ? ` · ${esc(state.live_name)}` : ""}</span>`);
  } else {
    chips.push(`<span class="chip"><span class="dot"></span>${t("chipLoggedOut")}</span>`);
  }

  const cards = state.accounts
    .map((a) => {
      const active = a.is_active;
      const hcls = a.health === "ok" ? "ok" : a.health;
      const hlabel = t(`health${a.health.charAt(0).toUpperCase()}${a.health.slice(1)}`);
      const meta = t("cardMeta", { created: esc(a.created_at), updated: esc(a.updated_at) });
      const snap = a.snapshot_ok
        ? `<span class="chip-checkin none">${t("snapshotSize", { size: fmtSize(a.snapshot_size) })}</span>`
        : `<span class="chip-checkin miss">${t("snapshotMissing")}</span>`;
      return `
      <div class="card ${active ? "active" : ""}" data-id="${esc(a.id)}">
        <div class="card-top">
          <span class="card-name">${esc(a.name)}</span>
          ${active ? `<span class="tag-active">${t("currentTag")}</span>` : ""}
          <span class="health ${hcls}" title="${esc(healthInfo(a))}"><span class="dot"></span>${esc(hlabel)}${daysText(a.token_days_left) ? ` · ${daysText(a.token_days_left)}` : ""}</span>
        </div>
      <div class="card-sub">${meta}</div>
      <div class="card-chips">${checkinChip(a)}${snap}</div>
      <div class="card-actions">
        ${active ? "" : `<button class="mini-btn primary" data-act="switch">${ic("switch", 12)}${t("btnSwitch")}</button>`}
        <button class="mini-btn" data-act="checkin" title="${t("btnCheckin")}">${ic("cal", 12)}${t("btnCheckin")}</button>
        <button class="mini-btn" data-act="rename">${ic("edit", 12)}${t("btnRename")}</button>
        <span class="spacer"></span>
        <button class="mini-btn danger" data-act="delete">${ic("trash", 12)}${t("btnDelete")}</button>
      </div>
      </div>`;
    })
    .join("");

  const empty =
    state.accounts.length === 0
      ? `<div class="empty">
          <img class="big" src="${lobsterLogo}" alt="" />
          <p><b>${t("emptyTitle")}</b></p>
          <p>${t("emptyLine1")}</p>
          <p>${t("emptyLine2")}</p>
          <p>${t("emptyLine3")}</p>
        </div>`
      : "";

  $app.innerHTML = `
    <div class="header">
      <img class="h-logo" src="${lobsterLogo}" alt="" />
      <span class="h-title">${t("appTitle")}</span>
      <span class="h-sub">${t("appSub")}</span>
      <span class="h-spacer"></span>
      <button class="icon-btn" data-act="lang" title="Language">${ic("globe", 15)}</button>
      <button class="icon-btn" data-act="settings" title="${t("setTitle")}">${ic("gear", 15)}</button>
    </div>
    <div class="status-row">${chips.join("")}</div>
    <div class="content">
      <div class="toolbar">
        <button class="btn btn-primary" data-act="capture">${ic("userPlus", 14)}${t("captureCurrent")}</button>
        <button class="btn" data-act="wizard">${ic("plus", 14)}${t("addNewAccount")}</button>
        ${state.lobster_running
          ? `<button class="btn" data-act="close">${ic("stop", 14)}${t("closeLobster")}</button>`
          : `<button class="btn" data-act="open" ${state.lobster_path_ok ? "" : "disabled"}>${ic("play", 14)}${t("launchLobster")}</button>`}
        <button class="btn" data-act="checkinAll">${ic("cal", 14)}${t("checkinAll")}</button>
      </div>
      <div class="cards">${proxyCard()}${cards}</div>
      ${empty}
    </div>`;
}

$app.addEventListener("click", (e) => {
  const btn = e.target.closest("[data-act]");
  if (!btn) return;
  const act = btn.dataset.act;
  const card = btn.closest(".card");
  const id = card?.dataset.id;
  const acc = state?.accounts.find((a) => a.id === id);

  if (act === "capture") return guard(doCapture);
  if (act === "wizard") return guard(openWizard);
  if (act === "open") return guard(doLaunch);
  if (act === "close") return guard(doClose);
  if (act === "checkinAll") return guard(doCheckinAll);
  if (act === "settings") return guard(openSettings);
  if (act === "lang") return guard(toggleLang);
  if (act === "proxyStart") return guard(doProxyStart);
  if (act === "proxyStop") return guard(doProxyStop);
  if (act === "proxyLog") return guard(showProxyLog);
  if (act === "register") return guard(doRegister);
  if (!acc) return;
  if (act === "switch") return guard(() => doSwitch(acc));
  if (act === "checkin") return guard(() => doCheckinOne(acc));
  if (act === "rename") return guard(() => doRename(acc));
  if (act === "delete") return guard(() => doDelete(acc));
});

function askConfirm(m) {
  return new Promise((resolve) => {
    openConfirmModal({ ...m, onYes: () => resolve(true) });
    document.querySelector(".cm-mask .cm-no")?.addEventListener("click", () => resolve(false));
    document.querySelector(".cm-mask")?.addEventListener("click", () => resolve(false));
  });
}

async function doCapture() {
  if (state.lobster_running) {
    const go = await askConfirm({
      title: t("confirmCaptureTitle"),
      desc: t("confirmCaptureDesc"),
    });
    if (!go) return;
  }
  const r = await invoke("capture_current");
  await refresh();
  for (const w of r.warnings || []) toast(w, "info");
  toast(t("captured", { name: r.account.name }));
  if (r.was_running && state.launch_after_switch) {
    invoke("launch_lobster")
      .then(() => refresh())
      .catch(() => {});
  }
}

async function doSwitch(acc) {
  if (state.lobster_running) {
    const go = await askConfirm({
      title: t("confirmSwitchTitle", { name: acc.name }),
      desc: t("confirmSwitchDesc"),
    });
    if (!go) return;
  }
  const r = await invoke("switch_to", { id: acc.id, force: true });
  await refresh();
  if (r.already_active) {
    toast(t("switchAlready", { name: r.name }), "info");
    return;
  }
  for (const w of r.warnings || []) toast(w, "info");
  toast(r.launched ? t("switchedLaunched") : t("switched", { name: r.name }));
}

async function doCheckinAll() {
  const n = state?.accounts.length || 0;
  if (n === 0) return;
  const r = await invoke("checkin", {});
  await refresh();
  toast(t("checkinDone", { ok: r.ok_count, done: r.done_count, err: r.error_count }), r.error_count > 0 ? "info" : "ok");
}

async function doCheckinOne(acc) {
  const r = await invoke("checkin", { id: acc.id });
  await refresh();
  const item = (r.results || [])[0];
  toast(item ? `${acc.name}: ${item.msg}` : t("checkinDone", { ok: r.ok_count, done: r.done_count, err: r.error_count }), item?.status === "error" ? "err" : "ok");
}

async function doRename(acc) {
  await new Promise((resolve) => {
    openInputModal({
      title: t("renameTitle"),
      value: acc.name,
      placeholder: t("renamePlaceholder"),
      onOk: async (name) => {
        if (!name || name === acc.name) return;
        try {
          await invoke("rename_account", { id: acc.id, name });
          toast(t("renamed"));
          await refresh();
        } catch (e) {
          toast(stripErr(e), "err");
        }
      },
    });
    document.querySelector(".in-mask .in-cancel")?.addEventListener("click", () => resolve());
    document.querySelector(".in-mask .in-go")?.addEventListener("click", () => setTimeout(resolve, 50));
  });
}

async function doDelete(acc) {
  const go = await askConfirm({
    title: t("confirmDeleteTitle", { name: acc.name }),
    desc: t("confirmDeleteDesc"),
    kind: "danger",
  });
  if (!go) return;
  await invoke("delete_account", { id: acc.id });
  await refresh();
  toast(t("deleted"));
}

async function doLaunch() {
  await invoke("launch_lobster");
  await refresh();
  toast(t("launched"));
}

async function doClose() {
  const go = await askConfirm({
    title: t("confirmKillTitle"),
    desc: t("confirmKillDesc"),
  });
  if (!go) return;
  await invoke("kill_lobster");
  await refresh();
  toast(t("killed"));
}

async function doProxyStart() {
  const st = await invoke("proxy_start");
  await refresh();
  toast(t("proxyStarted", { port: st.port }));
}

async function doProxyStop() {
  await invoke("proxy_stop");
  await refresh();
  toast(t("proxyStopped"));
}

async function showProxyLog() {
  document.querySelector(".pd-mask")?.remove();
  const mask = document.createElement("div");
  mask.className = "pd-mask";
  const r = await invoke("proxy_log");
  const tail = r?.tail || "(empty)";
  mask.innerHTML = `
    <div class="pd-panel pl-panel">
      <div class="pd-head">
        <span class="pd-title">${ic("doc", 16)}${t("proxyLog")}</span>
        <button class="icon-btn pd-close" title="${esc(t("commonClose"))}">${ic("x", 14)}</button>
      </div>
      <pre class="pl-log">${esc(tail)}</pre>
    </div>`;
  document.body.appendChild(mask);
  mask.addEventListener("click", (e) => {
    if (e.target === mask) mask.remove();
  });
  mask.querySelector(".pd-close").addEventListener("click", () => mask.remove());
}

async function doRegister() {
  const r = await invoke("register_ccswitch", { switchToIt: false });
  await refresh();
  toast(t("registered", { url: r.base_url }), "ok", `provider: ${r.provider} · model: ${r.model}`);
}

function openWizard() {
  return new Promise((resolve) => {
    document.querySelector(".wz-mask")?.remove();
    const mask = document.createElement("div");
    mask.className = "wz-mask";
    const done = () => {
      mask.remove();
      resolve();
    };
    mask.innerHTML = `
      <div class="wz-panel">
        <div class="wz-title">${t("wizTitle")}</div>
        <ul class="wz-steps">
          <li>${t("wizStep1")}</li>
          <li>${t("wizStep2")}</li>
          <li>${t("wizStep3")}</li>
        </ul>
        <div class="wz-actions">
          <button class="btn btn-ghost wz-cancel">${t("wizCancel")}</button>
          <button class="btn btn-primary wz-go">${t("wizGo")}</button>
        </div>
      </div>`;
    document.body.appendChild(mask);
    mask.querySelector(".wz-cancel").addEventListener("click", done);
    mask.querySelector(".wz-go").addEventListener("click", () => {
      mask.querySelector(".wz-panel").innerHTML = `
        <div class="wz-title">${t("wizTitle")}</div>
        <div class="wz-steps" style="margin-bottom:16px">${t("wizWaiting")}</div>
        <div class="wz-actions">
          <button class="btn btn-ghost wz-cancel">${t("wizCancel")}</button>
          <button class="btn btn-primary wz-cap">${ic("play", 13)}${t("wizCapture")}</button>
        </div>`;
      mask.querySelector(".wz-cancel").addEventListener("click", done);
      mask.querySelector(".wz-cap").addEventListener("click", async () => {
        const cap = mask.querySelector(".wz-cap");
        cap.disabled = true;
        try {
          const r = await invoke("capture_current");
          await refresh();
          for (const w of r.warnings || []) toast(w, "info");
          toast(t("captured", { name: r.account.name }));
          if (r.was_running && state.launch_after_switch) {
            invoke("launch_lobster").catch(() => {});
          }
        } catch (e) {
          toast(stripErr(e), "err");
        }
        done();
      });
      invoke("kill_lobster")
        .then(() => {
          if (state.lobster_path_ok) return invoke("launch_lobster").catch(() => {});
        })
        .then(() => refresh().catch(() => {}));
    });
  });
}

async function openSettings() {
  document.querySelector(".st-mask")?.remove();
  const mask = document.createElement("div");
  mask.className = "st-mask";
  const st = await invoke("get_state");
  const autostart = (await invoke("proxy_status")) || {};
  mask.innerHTML = `
    <div class="st-panel">
      <div class="st-title">${t("setTitle")}</div>
      <div class="st-row">
        <div class="st-label">${t("setPathLabel")}</div>
        <div class="st-path">
          <input class="st-input" id="setPath" type="text" placeholder="${t("setPathPlaceholder")}" value="${esc(st.lobster_path || "")}">
          <button class="btn" id="setPathPick">${t("setPathPick")}</button>
        </div>
        <div class="st-hint">${t("setPathHint")}</div>
      </div>
      <div class="st-row">
        <label class="st-check"><input type="checkbox" id="setLaunch" ${st.launch_after_switch ? "checked" : ""}>${t("setLaunchLabel")}</label>
      </div>
      <div class="st-row">
        <label class="st-check"><input type="checkbox" id="setAutostart"> ${t("setAutostartLabel")}</label>
      </div>
      <div class="st-row">
        <div class="st-label">${t("setLangLabel")}</div>
        <select class="st-input" id="setLang">
          <option value="zh" ${lang() === "zh" ? "selected" : ""}>中文</option>
          <option value="en" ${lang() === "en" ? "selected" : ""}>English</option>
        </select>
      </div>
      <div class="st-actions">
        <button class="btn btn-ghost st-cancel">${t("commonCancel")}</button>
        <button class="btn btn-primary st-save">${t("setSave")}</button>
      </div>
    </div>`;
  document.body.appendChild(mask);
  // autostart 状态需要从 settings 读，get_state 未含；先用 proxy_status 占位禁用态修复
  mask.querySelector("#setAutostart").checked = !!autostart.autostart;
  mask.addEventListener("click", (e) => {
    if (e.target === mask) mask.remove();
  });
  mask.querySelector(".st-cancel").addEventListener("click", () => mask.remove());
  mask.querySelector("#setPathPick").addEventListener("click", async () => {
    const r = await invoke("pick_lobster_path");
    if (r?.picked) mask.querySelector("#setPath").value = r.path;
  });
  mask.querySelector(".st-save").addEventListener("click", async () => {
    const btn = mask.querySelector(".st-save");
    btn.disabled = true;
    try {
      await invoke("set_settings", {
        lobsterPath: mask.querySelector("#setPath").value.trim(),
        launchAfterSwitch: mask.querySelector("#setLaunch").checked,
        language: mask.querySelector("#setLang").value,
        proxyAutostart: mask.querySelector("#setAutostart").checked,
      });
      mask.remove();
      await refresh();
      toast(t("setSaved"));
    } catch (e) {
      btn.disabled = false;
      toast(stripErr(e), "err");
    }
  });
}

async function toggleLang() {
  const next = lang() === "zh" ? "en" : "zh";
  await invoke("set_settings", { language: next });
  await refresh();
  toast(t("langChanged"));
}

listen("state-changed", () => refresh().catch(() => {})).catch(() => {});
setInterval(() => refresh().catch(() => {}), 20000);
document.addEventListener("focus", () => refresh().catch(() => {}));

refresh()
  .then(() => {
    dismissSplash();
  })
  .catch((e) => {
    dismissSplash();
    $app.innerHTML = `<div class="empty" style="margin:40px 18px"><div class="big">⚠️</div><p>${esc(stripErr(e))}</p></div>`;
  });
