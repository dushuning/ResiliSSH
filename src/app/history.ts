// 传输历史与断点：列表渲染、一键复用、继续未完成的传输。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload, prepareConnection, updateAuthFields, updateConnectionSummary } from "./connection";
import { showConfirm } from "./dialogs";
import { authTypeSelect, historyList, hostInput, keyPathInput, localPathInput, navHistoryBadge, pendingList, pendingSection, portInput, profileSelect, remotePathInput, statusText, transferBtn, usernameInput } from "./dom";
import { fileNameOf, formatBytes, formatTime } from "./format";
import { showPage } from "./nav";
import { selectLocalPathsExpanded, selectRemotePathsExpanded, updateDropZoneState, updateResolvedPathHint } from "./paths";
import { clearProbeResult, scheduleBackgroundProbe, updateTransferStatePanel } from "./probe";
import { setProgressState, setStatus } from "./progress";
import { applyProfile, findProfileFor, restoreCredentialsFromProfile, setJumpSelection } from "./servers";
import { state } from "./state";
import { getTransferredBytes, isDownloadMode, setTransferDirection, showTransferView } from "./transfer";
import { DownloadCheckpoint, TransferDirection, UploadCheckpoint, UploadHistoryEntry } from "./types";

export const HISTORY_STATUS_LABEL: Record<string, string> = {
  completed: t("完成"),
  failed: t("失败"),
  cancelled: t("已取消"),
  partial: t("部分成功"),
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
      const batch = entry.batch;
      const name = batch?.label ?? fileNameOf(direction === "download" ? entry.remote_path : entry.local_path);
      const retries = entry.retry_count > 0 ? t(" · 重连 {retry_count} 次", { retry_count: entry.retry_count }) : "";
      // 批量记录写出成功了几个：「完成」「失败」的状态标签本身说明不了部分成功的程度。
      const count = batch
        ? batch.done_count === batch.file_count
          ? t("{count} 个文件", { count: batch.file_count }) + " · "
          : t("{done} / {count} 个文件成功", { done: batch.done_count, count: batch.file_count }) + " · "
        : "";
      const meta = `${entry.username}@${entry.host} · ${count}${formatBytes(entry.file_size)} · ${formatTime(entry.finished_at)}${retries}`;
      const chip = document.createElement("span");
      const partial = Boolean(batch && entry.status === "failed" && batch.done_count > 0);
      const status = partial ? "partial" : entry.status;
      chip.className = `status-chip ${status}`;
      chip.textContent = HISTORY_STATUS_LABEL[status] ?? entry.status;
      if (entry.message) chip.title = entry.message;
      row.append(
        entryIcon(direction),
        entryMain(name, meta),
        chip,
        batch && batch.roots.length === 0
          ? reuseUnavailableNote()
          : makeButton(t("复用"), "btn btn-secondary btn-small", () => void reuseHistoryEntry(entry)),
      );
      return row;
    }),
  );
}


function reuseUnavailableNote(): HTMLSpanElement {
  const note = document.createElement("span");
  note.className = "entry-note";
  note.textContent = t("选择过多，无法复用");
  return note;
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
    setJumpSelection(null);
    updateConnectionSummary();
  }
  showTransferView("compose");
  showPage("transfer");
  if (entry.batch) {
    await reuseBatch(entry);
    return;
  }
  localPathInput.value = entry.local_path;
  remotePathInput.value = entry.remote_path;
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
}


/** 按当初选择的路径重新展开：文件夹里新增或删掉的文件都会如实反映。 */
async function reuseBatch(entry: UploadHistoryEntry) {
  const roots = entry.batch?.roots ?? [];
  if (entry.direction === "download") {
    localPathInput.value = entry.local_path;
    await selectRemotePathsExpanded(roots);
  } else {
    remotePathInput.value = entry.remote_path;
    await selectLocalPathsExpanded(roots);
  }
  updateResolvedPathHint();
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
      ? t("确定清除下载断点？\n\n下次下载将从头开始，已下载的部分（临时文件）会一并删除；本地原有的同名文件不受影响。")
      : t("确定清除上传断点？\n\n下次上传将从头开始，服务器上已传的部分（临时文件）会一并删除；远端原有的同名文件不受影响。"),
  );
  if (!confirmed) return;

  if (download) {
    await invoke("clear_saved_download_checkpoint");
    state.activeDownloadCheckpoint = null;
    updateTransferStatePanel();
    setStatus(t("断点记录已清除"));
    return;
  }
  // 删除服务器上的临时文件需要连接；连接不成（如没有密码）也照常清除断点，只是提示残留位置。
  let connection = null;
  try {
    await prepareConnection();
    connection = connectionPayload();
  } catch (error) {
    console.warn("clear checkpoint without connection", error);
  }
  const outcome = await invoke<{ leftover: string | null }>("clear_saved_checkpoint", { request: connection });
  state.activeUploadCheckpoint = null;
  updateTransferStatePanel();
  setStatus(
    outcome.leftover
      ? t("断点记录已清除，但服务器上的临时文件 {path} 未能删除，可手动删除", { path: outcome.leftover })
      : t("断点记录已清除"),
    outcome.leftover ? "retry" : "normal",
  );
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

