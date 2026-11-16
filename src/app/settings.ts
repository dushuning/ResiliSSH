// 主题、界面语言与并行连接数等偏好，保存在 localStorage。
import { languagePreference } from "../i18n";
import { DEFAULT_PARALLEL_CONNECTIONS, PARALLEL_CONNECTION_CHOICES } from "./constants";
import { ThemePreference } from "./types";

export const THEME_STORAGE_KEY = "resiliss-theme";

export const CONNECTIONS_STORAGE_KEY = "resiliss-connections";

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



/** 没存过、或存的不是可选值之一（手改过、旧版本留下的）时用默认值，不把脏数据发给后端。 */
export function readParallelConnections(): number {
  const saved = Number(localStorage.getItem(CONNECTIONS_STORAGE_KEY));
  return PARALLEL_CONNECTION_CHOICES.some((choice) => choice === saved) ? saved : DEFAULT_PARALLEL_CONNECTIONS;
}


/** 只存不通知：每次启动传输时现读（见 transferPayload），进行中的文件不受影响，下一个文件起生效。 */
export function applyParallelConnections(count: number) {
  localStorage.setItem(CONNECTIONS_STORAGE_KEY, String(count));
}


export function initParallelConnections() {
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="parallelConnections"][value="${readParallelConnections()}"]`,
  );
  if (radio) radio.checked = true;
}
