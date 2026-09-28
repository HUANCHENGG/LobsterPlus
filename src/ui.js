import { ic } from "./icons.js";
import { t } from "./i18n.js";

document.addEventListener("contextmenu", (e) => e.preventDefault());

export function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

export function dismissSplash() {
  const el = document.getElementById("splash");
  if (!el) return;
  el.style.opacity = "0";
  setTimeout(() => el.remove(), 220);
}

export function toast(msg, kind = "ok", detail = "") {
  let zone = document.querySelector(".toast-zone");
  if (!zone) {
    zone = document.createElement("div");
    zone.className = "toast-zone";
    document.body.appendChild(zone);
  }
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `<span class="t-ic">${ic(kind === "ok" ? "check" : "alert", 16)}</span>
    <span class="t-body">${esc(msg)}${detail ? `<span class="detail">${esc(detail)}</span>` : ""}</span>`;
  zone.appendChild(el);
  setTimeout(() => el.remove(), detail ? 5600 : 3200);
}

export function openConfirmModal(m) {
  document.querySelector(".cm-mask")?.remove();
  const kind = m.kind || "plain";
  const mask = document.createElement("div");
  mask.className = "cm-mask";
  mask.innerHTML = `
    <div class="cm-panel" role="alertdialog" aria-modal="true">
      <div class="cm-head">
        <span class="cm-ic ${kind}">${ic(m.icon || "alert", 20)}</span>
        <div class="cm-title">${esc(m.title)}</div>
      </div>
      ${m.desc ? `<div class="cm-desc">${esc(m.desc)}</div>` : ""}
      <div class="cm-actions">
        <button class="btn btn-ghost cm-no">${esc(m.noLabel || t("commonCancel"))}</button>
        <button class="btn cm-yes ${kind === "danger" ? "btn-danger" : "btn-primary"}">${esc(m.yesLabel || t("commonOk"))}</button>
      </div>
    </div>`;
  document.body.appendChild(mask);

  const panel = mask.querySelector(".cm-panel");
  const yes = mask.querySelector(".cm-yes");
  const no = mask.querySelector(".cm-no");
  const close = () => {
    document.removeEventListener("keydown", mask._key);
    mask.remove();
  };
  panel.addEventListener("click", (e) => e.stopPropagation());
  mask.addEventListener("click", close);
  no.addEventListener("click", close);
  yes.addEventListener("click", async () => {
    yes.disabled = true;
    no.disabled = true;
    try {
      await m.onYes?.();
    } finally {
      close();
    }
  });
  mask._key = (e) => {
    if (e.key === "Escape") close();
    if (e.key === "Enter" && e.target === document.body && !yes.disabled) yes.click();
  };
  document.addEventListener("keydown", mask._key);
  no.focus();
}

export function openInputModal(m) {
  document.querySelector(".in-mask")?.remove();
  const mask = document.createElement("div");
  mask.className = "in-mask";
  mask.innerHTML = `
    <div class="in-panel">
      <div class="in-title">${esc(m.title)}</div>
      <input class="in-input" type="text" placeholder="${esc(m.placeholder || "")}" value="${esc(m.value || "")}">
      <div class="in-actions">
        <button class="btn btn-ghost in-cancel">${esc(t("commonCancel"))}</button>
        <button class="btn btn-primary in-go">${esc(m.okLabel || t("commonOk"))}</button>
      </div>
    </div>`;
  document.body.appendChild(mask);

  const input = mask.querySelector(".in-input");
  const go = mask.querySelector(".in-go");
  const close = () => mask.remove();
  mask.addEventListener("click", (e) => {
    if (e.target === mask) close();
  });
  mask.querySelector(".in-cancel").addEventListener("click", close);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") go.click();
    if (e.key === "Escape") close();
  });
  go.addEventListener("click", async () => {
    go.disabled = true;
    try {
      await m.onOk?.(input.value.trim());
      close();
    } catch (e) {
      go.disabled = false;
      throw e;
    }
  });
  input.focus();
}
