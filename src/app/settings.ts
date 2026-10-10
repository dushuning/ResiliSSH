// 主题与界面语言偏好，保存在 localStorage。
import { languagePreference } from "../i18n";
import { ThemePreference } from "./types";

export const THEME_STORAGE_KEY = "resiliss-theme";

export const systemDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");


export function readThemePreference(): ThemePreference {
  const saved = localStorage.getItem(THEME_STORAGE_KEY);
  return saved === "light" || saved === "dark" ? saved : "system";
}


/** 「跟随系统」在这里解析成实际明暗写到 data-theme，CSS 只需维护一份深色变量。 */
export function resolveTheme(mode: ThemePreference): "light" | "dark" {
  if (mode !== "system") return mode;
  return systemDarkQuery.matches ? "dark" : "light";
}


export function applyThemePreference(mode: ThemePreference) {
  document.documentElement.dataset.theme = resolveTheme(mode);
  localStorage.setItem(THEME_STORAGE_KEY, mode);
}


export function initThemePreference() {
  const saved = readThemePreference();
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="themeMode"][value="${saved}"]`,
  );
  if (radio) radio.checked = true;
  applyThemePreference(saved);
  // 跟随系统时，系统切换明暗需要实时生效。
  systemDarkQuery.addEventListener("change", () => {
    if (readThemePreference() === "system") applyThemePreference("system");
  });
}


export function initLanguagePreference() {
  const saved = languagePreference();
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="languageMode"][value="${saved}"]`,
  );
  if (radio) radio.checked = true;
}

