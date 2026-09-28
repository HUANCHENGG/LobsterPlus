import { zh } from "./locales/zh.js";
import { en } from "./locales/en.js";

const TABLES = { zh, en };
let LANG = "zh";

export function init(lang) {
  LANG = lang === "en" ? "en" : "zh";
  document.documentElement.lang = localeTag();
}

export function lang() {
  return LANG;
}

export function localeTag() {
  return LANG === "en" ? "en-US" : "zh-CN";
}

export function t(key, params) {
  const table = TABLES[LANG] || zh;
  let s = table[key] ?? zh[key];
  if (s == null) {
    console.warn("[i18n] missing key:", key);
    s = key;
  }
  if (params) {
    for (const [k, v] of Object.entries(params)) s = s.replaceAll(`{${k}}`, String(v));
  }
  return s;
}

export function stripErr(e) {
  return typeof e === "string" ? e : String(e);
}
