// 纯函数工具：字节、速度、时间、路径的格式化与解析，不依赖任何界面状态。
import { locale, t } from "../i18n";

export function fileNameOf(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() ?? path;
}


/**
 * 多文件上传：逐个走单文件流程。某个文件失败不影响后面的文件；用户取消则整个队列停止。
 * 传完后命令只在队列结束后执行一次——部署场景下应当全部传完再重启服务。
 */
/** 目录路径统一以 / 结尾，拼子目录时不必再判断。 */
export function asDir(path: string): string {
  const trimmed = path.trim();
  return trimmed.endsWith("/") ? trimmed : `${trimmed}/`;
}


export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
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


export function formatProgressPercent(transferred: number, total: number): string {
  if (total <= 0) return "";
  const pct = Math.min(100, Math.round((transferred / total) * 100));
  return `${pct}%`;
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

