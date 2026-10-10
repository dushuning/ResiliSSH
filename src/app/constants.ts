// 界面层的调参常量，集中在一处，避免散落的魔法数字。
import { PageId } from "./types";

export const UPLOAD_CHUNK_BYTES = 1024 * 1024;

export const SPEED_SAMPLE_WINDOW_MS = 3000;

export const SPEED_REFRESH_MS = 1000;

export const SPEED_STALE_MS = 8_000;

export const UI_STALL_WARN_MS = 10_000;

export const CONFIRM_ANIM_MS = 200;

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

