// 纯函数工具：字节、速度、时间、路径的格式化与解析，不依赖任何界面状态。
import { locale, t } from "../i18n";

export function fileNameOf(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() ?? path;
}


/** 目录路径统一以 / 结尾，拼子目录时不必再判断。 */
export function asDir(path: string): string {
  const trimmed = path.trim();
  return trimmed.endsWith("/") ? trimmed : `${trimmed}/`;
}


export function formatBytes(bytes: number): string {
  // 速度是除出来的小数，不取整会显示成 166.66666666666666 B/s。
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}


export function formatTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString(locale(), {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}


/** 收藏的是目录：填的是文件路径时取其所在目录，统一以 / 结尾，便于直接作为上传目标。 */
export function remoteDirOf(path: string): string {
  const trimmed = path.trim();
  if (!trimmed) return "";
  if (trimmed.endsWith("/")) return trimmed;
  const slash = trimmed.lastIndexOf("/");
  return slash >= 0 ? trimmed.slice(0, slash + 1) : "";
}


/** 只取错误信息本身：直接 String(new Error(x)) 会在界面上带出英文 "Error: " 前缀。 */
export function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}


export function formatEtaSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 60) return t("{p1} 秒", { p1: Math.ceil(seconds) });
  if (seconds < 3600) return t("{p1} 分钟", { p1: Math.ceil(seconds / 60) });
  return t("{p1} 小时", { p1: (seconds / 3600).toFixed(1) });
}


const PERCENT_MAX = 100;

/**
 * 整数百分比，全应用统一向下取整：四舍五入会让 99.5% 显示成「100%」，而此时传输还没结束。
 * total 为 0（空文件）时返回 0，调用方不必各自防除零。
 */
export function floorPercent(done: number, total: number): number {
  if (total <= 0) return 0;
  return Math.max(0, Math.min(PERCENT_MAX, Math.floor((done / total) * PERCENT_MAX)));
}


export function formatProgressPercent(transferred: number, total: number): string {
  if (total <= 0) return "";
  return `${floorPercent(transferred, total)}%`;
}


/**
 * 断点里记录的路径与表单里填的路径是否指向同一个目标，与后端 path_matches_checkpoint 的规则一致：
 * 完全相同，或表单填的是「直接包含」断点文件的那个目录。不能放宽成任意上级目录：
 * 先传到 /data/sub/ 中断、再改传 /data/ 是另一个目标，后端会从头传，界面不能说「将续传」。
 */
export function pathCompatible(checkpointPath: string, formPath: string): boolean {
  const cp = checkpointPath.trim();
  const form = formPath.trim();
  // 表单还没填时不算匹配：否则空字符串会成为任何路径的「上级目录」。
  if (!form) return false;
  if (cp === form) return true;
  const base = form.replace(/\/+$/, "");
  return cp === base || cp === `${base}/${fileNameOf(cp)}`;
}


export function prefersReducedMotion(): boolean {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}


export function formatDuration(ms: number): string {
  const secs = Math.round(ms / 1000);
  if (secs < 60) return t("{secs} 秒", { secs });
  const mins = Math.floor(secs / 60);
  if (mins < 60) return t("{mins} 分 {p2} 秒", { mins, p2: secs % 60 });
  return t("{p1} 小时 {p2} 分", { p1: Math.floor(mins / 60), p2: mins % 60 });
}

