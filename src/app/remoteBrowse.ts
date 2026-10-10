// 远端目录浏览对话框：选目录（上传）或多选文件（下载）。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload, ensureCredentialsLoaded, prepareConnection, validateConnectionBeforeTransfer } from "./connection";
import { CONFIRM_ANIM_MS } from "./constants";
import { browseRemoteBtn, remoteBrowseCreateDirBtn, remoteBrowseGoBtn, remoteBrowseHint, remoteBrowseList, remoteBrowseNewDirName, remoteBrowseNewDirRow, remoteBrowseOverlay, remoteBrowsePath, remoteBrowseSelectDirBtn, remoteBrowseStatus, remoteBrowseTitle, remoteBrowseUpBtn, remotePathInput } from "./dom";
import { describeError, formatBytes } from "./format";
import { updateResolvedPathHint } from "./paths";
import { clearProbeResult, scheduleBackgroundProbe } from "./probe";
import { setStatus } from "./progress";
import { state } from "./state";
import { isDownloadMode } from "./transfer";
import { RemoteDirEntry, RemoteDirListing } from "./types";

export function updateRemoteBrowseModeUi() {
  const download = isDownloadMode();
  remoteBrowseTitle.textContent = download ? t("浏览远端文件") : t("浏览远端目录");
  remoteBrowseSelectDirBtn.classList.toggle("hidden", download);
  remoteBrowseSelectDirBtn.textContent = t("选择此目录");
  remoteBrowseHint.textContent = download
    ? t("双击文件夹进入；单击文件可多选，双击文件只选这一个")
    : t("双击文件夹进入；双击文件选中");
}


export function openRemoteBrowseOverlay() {
  remoteBrowseOverlay.classList.remove("hidden", "is-visible");
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      remoteBrowseOverlay.classList.add("is-visible");
    });
  });
}


export function closeRemoteBrowseOverlay() {
  remoteBrowseOverlay.classList.remove("is-visible");
  window.setTimeout(() => {
    remoteBrowseOverlay.classList.add("hidden");
    remoteBrowseList.replaceChildren();
    remoteBrowseStatus.textContent = "";
    state.remoteBrowseCurrent = null;
    hideRemoteBrowseNewDirForm();
  }, CONFIRM_ANIM_MS);
}


export function showRemoteBrowseNewDirForm() {
  remoteBrowseNewDirRow.classList.remove("hidden");
  remoteBrowseNewDirName.value = "";
  remoteBrowseNewDirName.focus();
}


export function hideRemoteBrowseNewDirForm() {
  remoteBrowseNewDirRow.classList.add("hidden");
  remoteBrowseNewDirName.value = "";
}


export function setRemoteBrowseStatus(message: string, kind: "normal" | "error" = "normal") {
  remoteBrowseStatus.textContent = message;
  remoteBrowseStatus.classList.toggle("error", kind === "error");
}


/** 下载模式下在当前目录勾选的远端文件；切换目录时清空，避免把不同目录的同名文件下到同一处互相覆盖。 */
export const remoteBrowsePicked = new Set<string>();


export function updateRemoteBrowsePickButton() {
  if (!isDownloadMode()) return;
  const count = remoteBrowsePicked.size;
  remoteBrowseSelectDirBtn.classList.toggle("hidden", count === 0);
  remoteBrowseSelectDirBtn.textContent = t("选择 {count} 个文件", { count });
}


export function renderRemoteBrowseListing(listing: RemoteDirListing) {
  remoteBrowsePicked.clear();
  updateRemoteBrowsePickButton();
  state.remoteBrowseCurrent = listing;
  remoteBrowsePath.value = listing.path;
  remoteBrowseUpBtn.disabled = !listing.parent;

  remoteBrowseList.replaceChildren();
  if (listing.entries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "remote-browse-empty";
    empty.textContent = t("此目录为空");
    remoteBrowseList.appendChild(empty);
    return;
  }

  for (const entry of listing.entries) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = `remote-browse-item${entry.is_dir ? " is-dir" : " is-file"}`;
    row.dataset.path = entry.path;
    row.dataset.isDir = entry.is_dir ? "1" : "0";

    const icon = document.createElement("span");
    icon.className = "remote-browse-icon";
    // 图标是本地静态 SVG 常量，不含远端数据，可安全用 innerHTML。
    icon.innerHTML = entry.is_dir ? icons.folder : icons.file;

    const name = document.createElement("span");
    name.className = "remote-browse-name";
    name.textContent = entry.name;

    const meta = document.createElement("span");
    meta.className = "remote-browse-meta";
    meta.textContent = entry.is_dir ? t("目录") : formatBytes(entry.size);

    row.append(icon, name, meta);
    row.addEventListener("click", () => {
      // 下载模式：单击文件切换多选，双击仍是「只要这一个」的快捷方式。
      if (!entry.is_dir && isDownloadMode()) {
        if (remoteBrowsePicked.has(entry.path)) remoteBrowsePicked.delete(entry.path);
        else remoteBrowsePicked.add(entry.path);
        row.classList.toggle("selected", remoteBrowsePicked.has(entry.path));
        updateRemoteBrowsePickButton();
        return;
      }
      remoteBrowseList
        .querySelectorAll(".remote-browse-item.selected")
        .forEach((el) => el.classList.remove("selected"));
      row.classList.add("selected");
    });
    row.addEventListener("dblclick", (event) => {
      event.preventDefault();
      void handleRemoteBrowseEntryActivate(entry);
    });
    remoteBrowseList.appendChild(row);
  }
}


export async function handleRemoteBrowseEntryActivate(entry: RemoteDirEntry) {
  if (entry.is_dir) {
    await loadRemoteBrowseDir(entry.path);
    return;
  }

  if (isDownloadMode()) {
    applyRemoteBrowseSelection(entry.path, false);
    return;
  }

  remotePathInput.value = entry.path;
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  closeRemoteBrowseOverlay();
}


export async function createRemoteBrowseDir() {
  const dirName = remoteBrowseNewDirName.value.trim();
  if (!dirName) {
    setRemoteBrowseStatus(t("请输入目录名称"), "error");
    return;
  }

  const parentPath = state.remoteBrowseCurrent?.path ?? remoteBrowsePath.value.trim();
  if (!parentPath) {
    setRemoteBrowseStatus(t("请先进入父目录"), "error");
    return;
  }

  remoteBrowseCreateDirBtn.disabled = true;
  setRemoteBrowseStatus(t("正在创建…"));

  try {
    await prepareConnection();
    const listing = await invoke<RemoteDirListing>("create_remote_dir", {
      request: {
        ...connectionPayload(),
        parent_path: parentPath,
        dir_name: dirName,
      },
    });
    hideRemoteBrowseNewDirForm();
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(t("已创建「{dirName}」", { dirName }));
  } catch (error) {
    setRemoteBrowseStatus(describeError(error), "error");
  } finally {
    remoteBrowseCreateDirBtn.disabled = false;
  }
}


export async function loadRemoteBrowseDir(path?: string) {
  if (state.remoteBrowseLoading) return;
  state.remoteBrowseLoading = true;
  browseRemoteBtn.disabled = true;
  remoteBrowseGoBtn.disabled = true;
  remoteBrowseUpBtn.disabled = true;
  setRemoteBrowseStatus(t("正在加载…"));

  try {
    await prepareConnection();
    const listing = await invoke<RemoteDirListing>("list_remote_dir", {
      request: {
        ...connectionPayload(),
        path: path?.trim() || remoteBrowsePath.value.trim() || null,
      },
    });
    renderRemoteBrowseListing(listing);
    setRemoteBrowseStatus(t("{length} 项", { length: listing.entries.length }));
  } catch (error) {
    setRemoteBrowseStatus(describeError(error), "error");
  } finally {
    state.remoteBrowseLoading = false;
    browseRemoteBtn.disabled = state.isTransferring;
    remoteBrowseGoBtn.disabled = false;
    if (state.remoteBrowseCurrent?.parent) {
      remoteBrowseUpBtn.disabled = false;
    }
  }
}


export function applyRemoteBrowseSelection(path: string, isDir: boolean) {
  if (isDownloadMode()) {
    if (isDir) return;
    remotePathInput.value = path;
  } else {
    remotePathInput.value = path.endsWith("/") ? path : `${path}/`;
  }
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  closeRemoteBrowseOverlay();
}


export async function openRemoteBrowseDialog() {
  if (state.isTransferring) return;

  try {
    await ensureCredentialsLoaded();
    validateConnectionBeforeTransfer();
  } catch (error) {
    setStatus(describeError(error), "error");
    return;
  }

  updateRemoteBrowseModeUi();
  hideRemoteBrowseNewDirForm();
  openRemoteBrowseOverlay();
  const initialPath = remotePathInput.value.trim() || undefined;
  remoteBrowsePath.value = initialPath ?? "";
  await loadRemoteBrowseDir(initialPath);
}

