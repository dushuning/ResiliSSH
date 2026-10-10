// 传输历史与断点：列表渲染、一键复用、继续未完成的传输。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { updateAuthFields, updateConnectionSummary } from "./connection";
import { showConfirm } from "./dialogs";
import { authTypeSelect, historyList, hostInput, keyPathInput, localPathInput, navHistoryBadge, pendingList, pendingSection, portInput, profileSelect, remotePathInput, statusText, transferBtn, usernameInput } from "./dom";
import { fileNameOf, formatBytes, formatTime } from "./format";
import { showPage } from "./nav";
import { updateDropZoneState, updateResolvedPathHint } from "./paths";
import { clearProbeResult, scheduleBackgroundProbe, updateTransferStatePanel } from "./probe";
import { setProgressState, setStatus } from "./progress";
import { applyProfile, findProfileFor, restoreCredentialsFromProfile } from "./servers";
import { state } from "./state";
import { getTransferredBytes, isDownloadMode, setTransferDirection, showTransferView } from "./transfer";
import { DownloadCheckpoint, TransferDirection, UploadCheckpoint, UploadHistoryEntry } from "./types";

export const HISTORY_STATUS_LABEL: Record<string, string> = {
  completed: t("完成"),
  failed: t("失败"),
  cancelled: t("已取消"),
};


export function entryIcon(direction: TransferDirection): HTMLSpanElement {
  const el = document.createElement("span");
  el.className = `entry-icon${direction === "download" ? " is-download" : ""}`;
  el.innerHTML = direction === "download" ? icons.download : icons.upload;
  return el;
}


export function entryMain(title: string, meta: string): HTMLDivElement {
  const main = document.createElement("div");
  main.className = "entry-main";
  const t = document.createElement("div");
  t.className = "entry-title";
  t.textContent = title;
  t.title = title;
  const m = document.createElement("div");
  m.className = "entry-meta";
  m.textContent = meta;
  m.title = meta;
  main.append(t, m);
  return main;
}


export function makeButton(label: string, className: string, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = className;
  btn.textContent = label;
  btn.addEventListener("click", onClick);
  return btn;
}


/**
 * 未完成的传输单独列在历史页顶部：断点以前只藏在传输页的提示条里，
 * 用户关掉应用再打开时很难发现「还能接着传」。
 */
export function renderPendingTransfers() {
  const items: { direction: TransferDirection; cp: UploadCheckpoint | DownloadCheckpoint }[] = [];
  // 正在传的那个方向不算「未完成」：它的断点就是当前任务本身，列出来只会让人以为有遗留任务。
  const running = state.isTransferring ? state.runDirection : null;
  if (state.activeUploadCheckpoint && running !== "upload") {
    items.push({ direction: "upload", cp: state.activeUploadCheckpoint });
  }
  if (state.activeDownloadCheckpoint && running !== "download") {
    items.push({ direction: "download", cp: state.activeDownloadCheckpoint });
  }

  navHistoryBadge.textContent = String(items.length);
  navHistoryBadge.classList.toggle("hidden", items.length === 0);
  pendingSection.classList.toggle("hidden", items.length === 0);

  pendingList.replaceChildren(
    ...items.map(({ direction, cp }) => {
      const row = document.createElement("div");
      row.className = "pending-item";
      const transferred = getTransferredBytes(cp);
      const name = fileNameOf(direction === "download" ? cp.remote_path : cp.local_path);
      const target = direction === "download" ? cp.local_path : `${cp.host}:${cp.remote_path}`;

      const progress = document.createElement("div");
      progress.className = "pending-progress";
      const track = document.createElement("div");
      track.className = "pending-track";
      const fill = document.createElement("div");
      fill.className = "pending-fill";
      fill.style.width = `${cp.file_size > 0 ? (transferred / cp.file_size) * 100 : 0}%`;
      track.append(fill);
      const label = document.createElement("span");
      label.textContent = `${formatBytes(transferred)} / ${formatBytes(cp.file_size)}`;
      progress.append(track, label);

      const actions = document.createElement("div");
      actions.className = "pending-actions";
      actions.append(
        makeButton(t("清除"), "btn btn-plain btn-small", () => void clearCheckpointFor(direction)),
        makeButton(t("继续"), "btn btn-primary btn-small", () => void resumePendingTransfer(direction)),
      );

      row.append(entryIcon(direction), entryMain(name, target), actions, progress);
      return row;
    }),
  );
}


export function renderHistory() {
  if (state.historyEntries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "list-empty";
    empty.textContent = t("还没有传输记录");
    historyList.replaceChildren(empty);
    return;
  }
  historyList.replaceChildren(
    ...state.historyEntries.map((entry) => {
      const direction: TransferDirection = entry.direction === "download" ? "download" : "upload";
      const row = document.createElement("div");
      row.className = "history-row";
      const name = fileNameOf(direction === "download" ? entry.remote_path : entry.local_path);
      const retries = entry.retry_count > 0 ? t(" · 重连 {retry_count} 次", { retry_count: entry.retry_count }) : "";
      const meta = `${entry.username}@${entry.host} · ${formatBytes(entry.file_size)} · ${formatTime(entry.finished_at)}${retries}`;
      const chip = document.createElement("span");
      chip.className = `status-chip ${entry.status}`;
      chip.textContent = HISTORY_STATUS_LABEL[entry.status] ?? entry.status;
      if (entry.message) chip.title = entry.message;
      row.append(
        entryIcon(direction),
        entryMain(name, meta),
        chip,
        makeButton(t("复用"), "btn btn-secondary btn-small", () => void reuseHistoryEntry(entry)),
      );
      return row;
    }),
  );
}


export async function reuseHistoryEntry(entry: UploadHistoryEntry) {
  if (state.isTransferring) return;
  setTransferDirection(entry.direction === "download" ? "download" : "upload");
  const profile = findProfileFor(entry.host, entry.port, entry.username);
  if (profile) {
    profileSelect.value = profile.id;
    await applyProfile(profile);
  } else {
    hostInput.value = entry.host;
    portInput.value = String(entry.port);
    usernameInput.value = entry.username;
    profileSelect.value = "";
    updateConnectionSummary();
  }
  localPathInput.value = entry.local_path;
  remotePathInput.value = entry.remote_path;
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  showTransferView("compose");
  showPage("transfer");
  scheduleBackgroundProbe();
}


export async function resumePendingTransfer(direction: TransferDirection) {
  if (state.isTransferring) return;
  setTransferDirection(direction);
  showTransferView("compose");
  showPage("transfer");
  // restore* 会把断点里的路径和服务器回填到表单，用户确认后点开始即可续传。
  if (direction === "download") await restoreDownloadCheckpoint();
  else await restoreUploadCheckpoint();
  transferBtn.focus();
}


export async function clearCheckpointFor(direction: TransferDirection) {
  const download = direction === "download";
  const confirmed = await showConfirm(
    t("清除断点"),
    download
      ? t("确定清除下载断点记录？\n\n清除后下次下载将从头开始，本地已下载的部分不会自动删除。")
      : t("确定清除断点记录？\n\n清除后下次上传将从头开始，已传到远端的数据不会自动删除。"),
  );
  if (!confirmed) return;

  if (download) {
    await invoke("clear_saved_download_checkpoint");
    state.activeDownloadCheckpoint = null;
  } else {
    await invoke("clear_saved_checkpoint");
    state.activeUploadCheckpoint = null;
  }
  updateTransferStatePanel();
  setStatus(t("断点记录已清除"));
}


export async function loadHistory() {
  state.historyEntries = await invoke<UploadHistoryEntry[]>("list_upload_history");
  renderHistory();
}


export async function refreshCheckpointFromDisk() {
  state.activeUploadCheckpoint = await invoke<UploadCheckpoint | null>("get_saved_checkpoint");
  if (!state.activeUploadCheckpoint || state.activeUploadCheckpoint.status === "completed") {
    state.activeUploadCheckpoint = null;
  }
  state.activeDownloadCheckpoint = await invoke<DownloadCheckpoint | null>(
    "get_saved_download_checkpoint",
  );
  if (!state.activeDownloadCheckpoint || state.activeDownloadCheckpoint.status === "completed") {
    state.activeDownloadCheckpoint = null;
  }
  updateTransferStatePanel();
}


export async function restoreUploadCheckpoint(options?: { keepStatus?: boolean }) {
  state.activeUploadCheckpoint = await invoke<UploadCheckpoint | null>("get_saved_checkpoint");
  if (!state.activeUploadCheckpoint || state.activeUploadCheckpoint.status === "completed") {
    state.activeUploadCheckpoint = null;
    if (!isDownloadMode()) updateTransferStatePanel();
    return;
  }

  if (isDownloadMode()) {
    updateTransferStatePanel();
    return;
  }

  const checkpoint = state.activeUploadCheckpoint;
  localPathInput.value = checkpoint.local_path;
  remotePathInput.value = checkpoint.remote_path;
  hostInput.value = checkpoint.host;
  portInput.value = String(checkpoint.port);
  usernameInput.value = checkpoint.username;
  authTypeSelect.value = checkpoint.auth_kind === "private_key" ? "key" : "password";
  if (checkpoint.key_path) keyPathInput.value = checkpoint.key_path;
  updateAuthFields();
  restoreCredentialsFromProfile(checkpoint.host, checkpoint.port, checkpoint.username);
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();

  setProgressState("idle");

  // 断点状态由传输页顶部的状态面板统一呈现，页脚不再重复一遍。
  if (!options?.keepStatus) statusText.textContent = "";
  updateTransferStatePanel();
}


export async function restoreDownloadCheckpoint() {
  state.activeDownloadCheckpoint = await invoke<DownloadCheckpoint | null>(
    "get_saved_download_checkpoint",
  );
  if (!state.activeDownloadCheckpoint || state.activeDownloadCheckpoint.status === "completed") {
    state.activeDownloadCheckpoint = null;
    if (isDownloadMode()) updateTransferStatePanel();
    return;
  }

  if (!isDownloadMode()) {
    updateTransferStatePanel();
    return;
  }

  const checkpoint = state.activeDownloadCheckpoint;
  localPathInput.value = checkpoint.local_path;
  remotePathInput.value = checkpoint.remote_path;
  hostInput.value = checkpoint.host;
  portInput.value = String(checkpoint.port);
  usernameInput.value = checkpoint.username;
  authTypeSelect.value = checkpoint.auth_kind === "private_key" ? "key" : "password";
  if (checkpoint.key_path) keyPathInput.value = checkpoint.key_path;
  updateAuthFields();
  restoreCredentialsFromProfile(checkpoint.host, checkpoint.port, checkpoint.username);
  updateDropZoneState();
  updateResolvedPathHint();
  updateConnectionSummary();

  setProgressState("idle");

  statusText.textContent = "";
  updateTransferStatePanel();
}


export async function restoreActiveCheckpoint() {
  await restoreUploadCheckpoint();
  await restoreDownloadCheckpoint();
}

