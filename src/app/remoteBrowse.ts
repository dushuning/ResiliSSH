// 远端目录浏览对话框：选目录（上传）或多选文件（下载）。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { invoke } from "@tauri-apps/api/core";
import { connectionPayload, ensureCredentialsLoaded, prepareConnection, validateConnectionBeforeTransfer } from "./connection";
import { CONFIRM_ANIM_MS } from "./constants";
import { browseRemoteBtn, remoteBrowseCreateDirBtn, remoteBrowseGoBtn, remoteBrowseHint, remoteBrowseList, remoteBrowseNewDirName, remoteBrowseNewDirRow, remoteBrowseOverlay, remoteBrowsePath, remoteBrowseSelectDirBtn, remoteBrowseShowHidden, remoteBrowseStatus, remoteBrowseTitle, remoteBrowseUpBtn, remotePathInput } from "./dom";
import { describeError, formatBytes } from "./format";
import { expandRemotePaths, reportExpansionWarnings, setSelectedDownloads, updateResolvedPathHint } from "./paths";
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
    ? t("双击文件夹进入；单击文件或文件夹可多选（文件夹会连同子目录一起下载），双击文件只选这一个")
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


/**
 * 下载模式下在当前目录勾选的远端条目（路径 → 是否文件夹）；切换目录时清空，
 * 避免把不同目录的同名文件下到同一处互相覆盖。
 */
export const remoteBrowsePicked = new Map<string, boolean>();


export function updateRemoteBrowsePickButton() {
  if (!isDownloadMode()) return;
  const count = remoteBrowsePicked.size;
  const hasDir = [...remoteBrowsePicked.values()].some(Boolean);
  remoteBrowseSelectDirBtn.classList.toggle("hidden", count === 0);
  remoteBrowseSelectDirBtn.textContent = hasDir
    ? t("选择 {count} 项", { count })
    : t("选择 {count} 个文件", { count });
}


/**
 * 确认下载选择。含文件夹时先让后端在服务器上递归展开：文件夹本身不能直接下载，
 * 队列需要的是逐个文件及其相对目录，才能在本地还原同样的结构。
 */
export async function applyRemoteDownloadPicks(picked: string[]) {
  if (picked.length === 0) return;
  if (!picked.some((path) => remoteBrowsePicked.get(path))) {
    // 带上列表里已知的大小，批量下载才能显示总进度。
    const sizes = new Map(state.remoteBrowseCurrent?.entries.map((e) => [e.path, e.size]) ?? []);
    setSelectedDownloads(
      picked,
      picked.map((path) => ({ path, rel_dir: "", size: sizes.get(path) ?? 0 })),
    );
    state.downloadRoots = [...picked];
    closeRemoteBrowseOverlay();
    return;
  }
  remoteBrowseSelectDirBtn.disabled = true;
  setRemoteBrowseStatus(t("正在列出文件夹内容…"));
  try {
    const expanded = await expandRemotePaths(picked);
    if (expanded.entries.length === 0) {
      setRemoteBrowseStatus(t("所选文件夹里没有可下载的文件"), "error");
      return;
    }
    setSelectedDownloads(
      expanded.entries.map((e) => e.path),
      expanded.entries,
    );
    state.downloadRoots = [...picked];
    closeRemoteBrowseOverlay();
    reportExpansionWarnings(expanded);
  } catch (error) {
    setRemoteBrowseStatus(describeError(error), "error");
  } finally {
    remoteBrowseSelectDirBtn.disabled = false;
  }
}


/** 「显示隐藏文件」的偏好跨次保留：需要它的人（如要下载 .env）每次都要找，很烦。 */
const SHOW_HIDDEN_STORAGE_KEY = "resiliss-remote-show-hidden";
remoteBrowseShowHidden.checked = localStorage.getItem(SHOW_HIDDEN_STORAGE_KEY) === "1";
remoteBrowseShowHidden.addEventListener("change", () => {
  localStorage.setItem(SHOW_HIDDEN_STORAGE_KEY, remoteBrowseShowHidden.checked ? "1" : "0");
  if (state.remoteBrowseCurrent) {
    renderRemoteBrowseListing(state.remoteBrowseCurrent);
    updateRemoteBrowseCount(state.remoteBrowseCurrent);
  }
});


/** 以点开头的文件（.bashrc、.cache 等）默认不显示：挑要传的文件时它们只是干扰。 */
function visibleEntries(listing: RemoteDirListing): RemoteDirEntry[] {
  return remoteBrowseShowHidden.checked
    ? listing.entries
    : listing.entries.filter((entry) => !entry.name.startsWith("."));
}


/** 状态行写明隐藏了多少项，免得用户以为文件不见了。 */
export function updateRemoteBrowseCount(listing: RemoteDirListing) {
  const shown = visibleEntries(listing).length;
  const hidden = listing.entries.length - shown;
  setRemoteBrowseStatus(
    hidden > 0
      ? t("{length} 项，另有 {hidden} 个隐藏项", { length: shown, hidden })
      : t("{length} 项", { length: shown }),
  );
}


export function renderRemoteBrowseListing(listing: RemoteDirListing) {
  remoteBrowsePicked.clear();
  updateRemoteBrowsePickButton();
  state.remoteBrowseCurrent = listing;
  remoteBrowsePath.value = listing.path;
  remoteBrowseUpBtn.disabled = !listing.parent;

  remoteBrowseList.replaceChildren();
  const entries = visibleEntries(listing);
  if (entries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "remote-browse-empty";
    empty.textContent = t("此目录为空");
    remoteBrowseList.appendChild(empty);
    return;
  }

  for (const entry of entries) {
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
      // 下载模式：单击文件或文件夹切换多选；双击文件夹仍是进入，双击文件是「只要这一个」。
      if (isDownloadMode()) {
        if (remoteBrowsePicked.has(entry.path)) remoteBrowsePicked.delete(entry.path);
        else remoteBrowsePicked.set(entry.path, entry.is_dir);
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
    updateRemoteBrowseCount(listing);
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

