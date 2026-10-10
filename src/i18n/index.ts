/**
 * 界面多语言。采用 gettext 风格：以中文原文作为 key，`t("开始上传")`。
 * 这样代码保持可读，翻译者和贡献者直接看到原文；新增语言只需新增一份字典。
 * 未翻译的条目回退为中文原文（scripts/check-i18n.mjs 会在检查时报出缺失项）。
 */
import { en } from "./en";

export type Language = "zh" | "en";
export type LanguagePreference = "system" | Language;

const LANGUAGE_STORAGE_KEY = "resiliss-language";

const dictionaries: Record<Exclude<Language, "zh">, Record<string, string>> = { en };

function readPreference(): LanguagePreference {
  const saved = localStorage.getItem(LANGUAGE_STORAGE_KEY);
  return saved === "zh" || saved === "en" ? saved : "system";
}

/** 跟随系统时：系统首选语言是中文才用中文，其余一律英文（英文覆盖面最广）。 */
function resolveLanguage(preference: LanguagePreference): Language {
  if (preference !== "system") return preference;
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

// 在模块加载时就确定语言：其他模块里有用 t() 初始化的常量（如状态标签表）。
const currentLanguage = resolveLanguage(readPreference());
document.documentElement.lang = currentLanguage === "zh" ? "zh-CN" : "en";

export function language(): Language {
  return currentLanguage;
}

export function languagePreference(): LanguagePreference {
  return readPreference();
}

/**
 * 切换语言后整页重新加载：界面文案散落在模板与各处状态文本里，逐个刷新容易漏。
 * 传输在后端进行，页面重载后会自动重新接上。
 */
export function setLanguagePreference(preference: LanguagePreference) {
  localStorage.setItem(LANGUAGE_STORAGE_KEY, preference);
  window.location.reload();
}

/** 日期、数字格式与界面语言保持一致。 */
export function locale(): string {
  return currentLanguage === "zh" ? "zh-CN" : "en-US";
}

/**
 * 翻译并替换 `{name}` 占位符。参数值按原样插入：调用方渲染时仍需用 textContent，
 * 不能因为经过了 t() 就当成可信 HTML。
 */
export function t(source: string, params?: Record<string, string | number>): string {
  const template =
    currentLanguage === "zh" ? source : (dictionaries[currentLanguage][source] ?? source);
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in params ? String(params[name]) : match,
  );
}
