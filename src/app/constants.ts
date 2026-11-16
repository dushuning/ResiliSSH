// 界面层的调参常量，集中在一处，避免散落的魔法数字。
import { PageId } from "./types";

export const UPLOAD_CHUNK_BYTES = 1024 * 1024;

export const SPEED_SAMPLE_WINDOW_MS = 3000;

export const SPEED_REFRESH_MS = 1000;

export const SPEED_STALE_MS = 8_000;

export const UI_STALL_WARN_MS = 10_000;

export const CONFIRM_ANIM_MS = 200;

/** 「已复制」反馈在按钮上停留的时长。 */
export const COPY_FEEDBACK_MS = 1500;

/**
 * 并行连接数的可选值与默认值。上限和默认值与后端一致（MAX_CONNECTIONS / DEFAULT_CONNECTIONS），
 * 修改时两边同步；后端还会再限制一次范围，并对小文件固定只用一条。
 */
export const PARALLEL_CONNECTION_CHOICES = [1, 2, 4, 8] as const;

export const DEFAULT_PARALLEL_CONNECTIONS = 4;

/**
 * 未完成批量存进 localStorage 的大小上限（字符数）。一次选择最多 10 000 个文件（后端 MAX_EXPANDED_FILES），
 * 按每个文件约 150 个字符估算是 1.5 MB 左右；这个上限能容纳它，同时离 WebView 约 5 MB 的配额还有余量，
 * 不会挤占主题、常用命令等其他设置。路径特别长而超出时就不保存（见 saveBatch）。
 */
export const BATCH_STORAGE_MAX_CHARS = 2_000_000;

/** SSH 默认端口：端口留空时使用，显示地址时等于它就省略不写。 */
export const DEFAULT_SSH_PORT = 22;

/** 路径预检仅在失焦或程序化改路径时触发，避免每次按键都连 SSH */
export const PROBE_DEBOUNCE_MS = 400;


export const PAGE_ORDER: readonly PageId[] = ["transfer", "servers", "history", "logs", "settings"];

export const TRANSFER_LOCK_SELECTORS = [
  "#profileSelect",
  "#profileName",
  "#saveProfileBtn",
  "#deleteProfileBtn",
  "#hostSelect",
  "#host",
  "#port",
  "#username",
  "#authType",
  "#keyPath",
  "#keyPassphrase",
  "#password",
  "#jumpProfile",
  "#pickKeyBtn",
  "#localPath",
  "#remotePath",
  "#browseRemoteBtn",
  "#pickFileBtn",
  "#pickFolderBtn",
  "#changeFileBtn",
  "#forceOverwrite",
  'input[name="transferDirection"]',
  'input[name="transferMode"]',
  "#newServerBtn",
  ".server-item",
  "#rememberSecrets",
  "#bookmarkBtn",
  "#pathMenuBtn",
  "#postCommand",
  "#saveCommandBtn",
  "#commandMenuBtn",
] as const;

