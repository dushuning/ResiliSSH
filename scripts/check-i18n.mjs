#!/usr/bin/env node
/**
 * 检查界面翻译是否完整：
 *  1. 代码里每个 t("…") 的原文在英文字典里都有翻译；
 *  2. 英文字典里没有已经不再使用的条目；
 *  3. 没有遗漏 t() 包裹的中文（注释除外，允许名单见 ALLOWED）。
 * 用法：npm run check:i18n（CI 中运行，失败时退出码非 0）。
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const SRC = new URL("../src/", import.meta.url).pathname;
/** 有意保留的中文：语言名需以本语言显示；与后端原文做匹配的关键字不能翻译。 */
const ALLOWED = ["中文", "已有下载任务在进行中", "已有上传任务在进行中", "远端路径错误", "远端路径「", "请填写密码", "认证失败", "服务器拒绝了提供的凭据", "SFTP 错误"];

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === "i18n" ? [] : walk(path);
    return path.endsWith(".ts") ? [path] : [];
  });
}

/** 去掉注释，避免把注释里的中文当成遗漏。 */
function stripComments(code) {
  return code.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:"'`])\/\/.*$/gm, "$1").replace(/<!--[\s\S]*?-->/g, "");
}

/** 把 JS 源码里的字符串字面量内容还原成运行时的字符串，才能与字典 key 对上。 */
function unescape(literal) {
  return JSON.parse(`"${literal.replace(/\\'/g, "'")}"`);
}

const used = new Set();
const leftovers = [];
for (const file of walk(SRC)) {
  const code = stripComments(readFileSync(file, "utf8"));
  for (const m of code.matchAll(/\bt\(\s*"((?:[^"\\]|\\.)*)"/g)) used.add(unescape(m[1]));
  const withoutT = code.replace(/\bt\(\s*"((?:[^"\\]|\\.)*)"/g, "t(\"\")");
  for (const m of withoutT.matchAll(/["'`]([^"'`\n]*[一-鿿][^"'`\n]*)["'`]/g)) {
    if (!ALLOWED.some((a) => m[1].includes(a))) leftovers.push(`${file.replace(SRC, "src/")}: ${m[1]}`);
  }
}

const enSource = readFileSync(join(SRC, "i18n/en.ts"), "utf8");
const translated = new Set(
  [...enSource.matchAll(/^\s*"((?:[^"\\]|\\.)*)"\s*:/gm)].map((m) => unescape(m[1])),
);

const missing = [...used].filter((k) => !translated.has(k));
const unused = [...translated].filter((k) => !used.has(k));
const report = (title, items) => items.length && console.log(`\n${title} (${items.length}):\n  ${items.join("\n  ")}`);
report("缺少英文翻译 / missing English", missing);
report("字典里多余的条目 / unused entries", unused);
report("未用 t() 包裹的中文 / untranslated literals", leftovers);
if (process.argv.includes("--list-missing")) {
  for (const k of missing) console.log(JSON.stringify(k));
}
const failed = missing.length + unused.length + leftovers.length;
console.log(failed ? `\n✗ i18n check failed` : `✓ i18n OK: ${used.size} strings translated`);
process.exit(failed ? 1 : 0);
