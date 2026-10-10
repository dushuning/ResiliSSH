// 本地文件/文件夹选择、拖放、远端目录收藏与最近使用，以及多选时的队列判定。
import { t } from "../i18n";
import { icons } from "../ui/icons";
import { type PopoverItem } from "../ui/popover";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { connectionPayload, prepareConnection } from "./connection";
import { bookmarkBtn, dropZone, fileChip, fileChipName, fileChipPath, hostInput, localPathInput, pathMenu, portInput, remotePathInput, resolvedPathHint, usernameInput } from "./dom";
import { asDir, describeError, fileNameOf, formatBytes, remoteDirOf } from "./format";
import { showPage } from "./nav";
import { clearProbeResult, scheduleBackgroundProbe } from "./probe";
import { setStatus } from "./progress";
import { RECENT_DIR_LIMIT, currentProfile, saveProfileExtras } from "./servers";
import { state } from "./state";
import { isDownloadMode, showTransferView } from "./transfer";
import { ExpandedPaths, LocalEntry } from "./types";

export function isQueueSelection(): boolean {
  // 文件夹里只有一个文件时也要走队列：它需要放进目标端的子目录。
  if (isDownloadMode()) return state.selectedDownloads.length > 1 || state.downloadRelDirs.size > 0;
  return state.selectedUploads.length > 1 || state.uploadRelDirs.size > 0;
}


/**
 * 文件夹展开后需要提醒用户的情况：超过上限被截断，或有子目录读不了。
 * 两种情况都意味着「不是整个文件夹都会传」，静默处理会让用户误以为全部送达。
 */
export function reportExpansionWarnings(expanded: ExpandedPaths) {
  const notes: string[] = [];
  if (expanded.truncated) {
    notes.push(t("文件太多，只取了前 {count} 个", { count: expanded.entries.length }));
  }
  if (expanded.unreadable_dirs.length > 0) {
    notes.push(
      t("{count} 个文件夹无法读取，其中的文件不会传输（详见日志）", {
        count: expanded.unreadable_dirs.length,
      }),
    );
  }
  if (notes.length > 0) setStatus(notes.join("；"), "retry");
}


export function setSelectedUploads(paths: string[], entries: LocalEntry[] = []) {
  state.selectedUploads = paths.filter(Boolean);
  state.uploadRelDirs = new Map(entries.filter((e) => e.rel_dir).map((e) => [e.path, e.rel_dir]));
  state.uploadSizes = new Map(entries.map((e) => [e.path, e.size]));
  localPathInput.value = state.selectedUploads[0] ?? "";
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  // 多文件时只预检第一个会产生误导，等开始时逐个预检。
  if (state.selectedUploads.length === 1) scheduleBackgroundProbe();
}


/** 拖入或选择的路径可能包含文件夹，交给后端递归展开后再作为待上传列表。 */
export async function selectLocalPathsExpanded(paths: string[]) {
  if (paths.length === 0) return;
  try {
    const expanded = await invoke<ExpandedPaths>("expand_local_paths", { paths });
    if (expanded.entries.length === 0) {
      setStatus(t("所选文件夹里没有可上传的文件"), "error");
      return;
    }
    setSelectedUploads(
      expanded.entries.map((e) => e.path),
      expanded.entries,
    );
    state.uploadRoots = [...paths];
    reportExpansionWarnings(expanded);
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}


/** 在服务器上展开远端路径（文件夹递归）。远端浏览确认选择、复用批量历史都走这里。 */
export async function expandRemotePaths(paths: string[]): Promise<ExpandedPaths> {
  await prepareConnection();
  return invoke<ExpandedPaths>("expand_remote_paths", {
    request: { ...connectionPayload(), paths },
  });
}


/** 复用批量下载历史：重新展开当初勾选的远端路径作为待下载列表。 */
export async function selectRemotePathsExpanded(paths: string[]) {
  if (paths.length === 0) return;
  try {
    const expanded = await expandRemotePaths(paths);
    if (expanded.entries.length === 0) {
      setStatus(t("所选文件夹里没有可下载的文件"), "error");
      return;
    }
    setSelectedDownloads(expanded.entries.map((e) => e.path), expanded.entries);
    state.downloadRoots = [...paths];
    reportExpansionWarnings(expanded);
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}


export function setSelectedDownloads(paths: string[], entries: LocalEntry[] = []) {
  state.selectedDownloads = paths.filter(Boolean);
  state.downloadRelDirs = new Map(entries.filter((e) => e.rel_dir).map((e) => [e.path, e.rel_dir]));
  state.downloadSizes = new Map(entries.map((e) => [e.path, e.size]));
  remotePathInput.value = state.selectedDownloads[0] ?? "";
  // 多个文件只能下载到目录里，否则会互相覆盖成同一个本地文件。
  const local = localPathInput.value.trim();
  if (isQueueSelection() && local && !local.endsWith("/")) {
    localPathInput.value = `${local}/`;
  }
  updateDropZoneState();
  updateResolvedPathHint();
  clearProbeResult();
  if (state.selectedDownloads.length === 1) scheduleBackgroundProbe();
}


export function updateBookmarkButton() {
  const profile = currentProfile();
  const dir = remoteDirOf(remotePathInput.value);
  const bookmarked = Boolean(profile && dir && profile.bookmarks.includes(dir));
  bookmarkBtn.disabled = !profile || !dir || state.isTransferring;
  bookmarkBtn.classList.toggle("is-active", bookmarked);
  bookmarkBtn.innerHTML = bookmarked ? icons.starFilled : icons.star;
  bookmarkBtn.title = !profile
    ? t("保存服务器后才能收藏目录")
    : !dir
      ? t("填写远端目录后可收藏")
      : bookmarked
        ? t("取消收藏 {dir}", { dir })
        : t("收藏 {dir}", { dir });
}


export async function toggleBookmark() {
  const profile = currentProfile();
  const dir = remoteDirOf(remotePathInput.value);
  if (!profile || !dir) return;
  const bookmarks = profile.bookmarks.includes(dir)
    ? profile.bookmarks.filter((b) => b !== dir)
    : [dir, ...profile.bookmarks];
  try {
    await saveProfileExtras(profile, { bookmarks });
    updateBookmarkButton();
  } catch (error) {
    setStatus(describeError(error), "error");
  }
}


/** 最近目录从传输历史推导，不额外存储：历史本来就记录了每次传到哪里。 */
export function recentDirsForCurrentServer(exclude: string[]): string[] {
  const host = hostInput.value.trim();
  const port = Number(portInput.value) || 22;
  const user = usernameInput.value.trim();
  const dirs: string[] = [];
  for (const entry of state.historyEntries) {
    if (entry.host !== host || entry.port !== port || entry.username !== user) continue;
    const dir = remoteDirOf(entry.remote_path);
    if (dir && !dirs.includes(dir) && !exclude.includes(dir)) dirs.push(dir);
    if (dirs.length >= RECENT_DIR_LIMIT) break;
  }
  return dirs;
}


export function useRemotePath(path: string) {
  remotePathInput.value = path;
  updateResolvedPathHint();
  clearProbeResult();
  scheduleBackgroundProbe();
  updateBookmarkButton();
  remotePathInput.focus();
}


export function openPathMenu() {
  const profile = currentProfile();
  const bookmarks = profile?.bookmarks ?? [];
  const bookmarkItems: PopoverItem[] = bookmarks.map((dir) => ({
    label: dir,
    onSelect: () => useRemotePath(dir),
    onRemove: () => {
      if (!profile) return;
      void saveProfileExtras(profile, { bookmarks: bookmarks.filter((b) => b !== dir) }).then(() => {
        updateBookmarkButton();
        openPathMenu();
      });
    },
  }));
  const recentItems: PopoverItem[] = recentDirsForCurrentServer(bookmarks).map((dir) => ({
    label: dir,
    onSelect: () => useRemotePath(dir),
  }));
  pathMenu.show(
    [
      { heading: t("收藏"), items: bookmarkItems },
      { heading: t("最近使用"), items: recentItems },
    ],
    profile
      ? t("还没有常用目录\n填好远端目录后点输入框里的 ☆ 收藏")
      : t("这台服务器还没保存\n到「服务器」页保存后即可收藏目录"),
  );
}


export function updateDropZoneState() {
  const path = localPathInput.value.trim();
  // 路径被程序直接改写（复用历史、恢复断点、手动输入）时，退回单文件选择。
  if (!isDownloadMode() && state.selectedUploads[0] !== path) {
    state.selectedUploads = path ? [path] : [];
    state.uploadRoots = path ? [path] : [];
    state.uploadRelDirs = new Map();
    state.uploadSizes = new Map();
  }
  const hasPath = path.length > 0;
  dropZone.classList.toggle("has-file", hasPath);
  fileChip.classList.toggle("hidden", !hasPath);
  dropZone.classList.toggle("hidden", hasPath);
  if (!hasPath) return;

  if (isQueueSelection() && !isDownloadMode()) {
    const folders = [...new Set([...state.uploadRelDirs.values()].map((d) => d.split("/")[0]))];
    const totalSize = [...state.uploadSizes.values()].reduce((sum, size) => sum + size, 0);
    // 来自同一个文件夹时以文件夹为主角显示，否则列出文件名。
    fileChipName.textContent =
      folders.length === 1 && state.uploadRelDirs.size === state.selectedUploads.length
        ? t("{folder}/ · {count} 个文件", { folder: folders[0], count: state.selectedUploads.length })
        : t("{length} 个文件", { length: state.selectedUploads.length });
    const names = state.selectedUploads.map((p) => {
      const rel = state.uploadRelDirs.get(p);
      return rel ? `${rel}/${fileNameOf(p)}` : fileNameOf(p);
    });
    fileChipPath.textContent =
      (totalSize > 0 ? `${formatBytes(totalSize)} · ` : "") + names.join("、");
    fileChipPath.title = names.join("\n");
  } else {
    fileChipName.textContent = fileNameOf(path);
    fileChipPath.textContent = path;
    fileChipPath.title = path;
  }
}


export function previewResolvedPath(): string | null {
  const remote = remotePathInput.value.trim();
  const local = localPathInput.value.trim();
  if (!remote || !local) return null;

  if (isDownloadMode()) {
    const remoteName = remote.split("/").pop() ?? "";
    if (!remoteName) return null;
    if (local.endsWith("/") || local.endsWith("\\")) {
      const base = local.replace(/[/\\]+$/, "");
      return `${base}/${remoteName}`;
    }
    const cp = state.activeDownloadCheckpoint;
    if (cp && cp.local_path !== local && cp.remote_path === remote) {
      return cp.local_path;
    }
    return null;
  }

  const fileName = local.split("/").pop() ?? "";
  if (!fileName) return null;

  if (remote.endsWith("/")) {
    return `${remote}${fileName}`;
  }

  if (
    state.activeUploadCheckpoint &&
    state.activeUploadCheckpoint.remote_path !== remote &&
    state.activeUploadCheckpoint.local_path === local
  ) {
    return state.activeUploadCheckpoint.remote_path;
  }

  return null;
}


export function updateResolvedPathHint() {
  // 所有改动远端路径的入口（浏览选择、复用历史、恢复断点）都会走到这里，顺带同步收藏星标。
  updateBookmarkButton();
  // 路径被程序直接改写（复用历史、恢复断点、手动输入）时，退回单文件选择。
  if (isDownloadMode() && state.selectedDownloads[0] !== remotePathInput.value.trim()) {
    const remote = remotePathInput.value.trim();
    state.selectedDownloads = remote ? [remote] : [];
    state.downloadRoots = remote ? [remote] : [];
    state.downloadRelDirs = new Map();
    state.downloadSizes = new Map();
  }
  if (isQueueSelection() && isDownloadMode()) {
    const dir = localPathInput.value.trim();
    const totalSize = [...state.downloadSizes.values()].reduce((sum, size) => sum + size, 0);
    resolvedPathHint.textContent =
      t("将依次下载 {count} 个文件到：{dir}", {
        count: state.selectedDownloads.length,
        dir: dir ? asDir(dir) : "…",
      }) + (totalSize > 0 ? ` · ${formatBytes(totalSize)}` : "");
    resolvedPathHint.title = state.selectedDownloads
      .map((p) => {
        const rel = state.downloadRelDirs.get(p);
        return rel ? `${rel}/${fileNameOf(p)}` : fileNameOf(p);
      })
      .join("\n");
    resolvedPathHint.classList.remove("hidden");
    return;
  }
  if (isQueueSelection()) {
    const dir = remotePathInput.value.trim();
    resolvedPathHint.textContent = dir
      ? t("将依次上传 {count} 个文件到：{dir}", {
          count: state.selectedUploads.length,
          dir: dir.endsWith("/") ? dir : `${dir}/`,
        })
      : "";
    resolvedPathHint.classList.toggle("hidden", !dir);
    return;
  }
  const resolved = previewResolvedPath();
  const remote = remotePathInput.value.trim();
  if (resolved && resolved !== remote && resolved !== localPathInput.value.trim()) {
    const label = isDownloadMode() ? t("实际保存至") : t("实际上传至");
    resolvedPathHint.textContent = `${label}：${resolved}`;
    resolvedPathHint.classList.remove("hidden");
  } else {
    resolvedPathHint.classList.add("hidden");
  }
}


export function setupDragDrop() {
  void getCurrentWebview().onDragDropEvent((event) => {
    // 拖入文件是整个窗口范围的：在别的页面拖进来也直接带到传输页，但传输中不打断。
    if (isDownloadMode() || state.isTransferring) return;
    if (event.payload.type === "over") {
      dropZone.classList.add("drag-over");
    } else if (event.payload.type === "leave") {
      dropZone.classList.remove("drag-over");
    } else if (event.payload.type === "drop") {
      dropZone.classList.remove("drag-over");
      const paths = event.payload.paths;
      if (paths.length > 0) {
        if (state.currentView !== "compose") showTransferView("compose");
        showPage("transfer");
        void selectLocalPathsExpanded(paths);
      }
    }
  });
}


/** 上传可多选（排队依次传）；下载选择的是本地保存目录，只能单选。 */
/** 上传可多选文件（排队依次传）；下载选择的是本地保存目录，只能单选。 */
export async function pickLocalPaths() {
  const download = isDownloadMode();
  const selected = await open({ multiple: !download, directory: download });
  if (selected === null) return;
  if (download) {
    localPathInput.value = asDir(selected as string);
    updateDropZoneState();
    updateResolvedPathHint();
    clearProbeResult();
    scheduleBackgroundProbe();
  } else {
    // 与拖入、选文件夹走同一条路：由后端展开并带回文件大小，批量传输才能显示总进度。
    await selectLocalPathsExpanded(Array.isArray(selected) ? selected : [selected]);
  }
}


/** macOS 的打开面板不能同时选文件和文件夹，所以文件夹单独一个入口；拖入时两者都支持。 */
export async function pickLocalFolders() {
  const selected = await open({ multiple: true, directory: true });
  if (selected === null) return;
  await selectLocalPathsExpanded(Array.isArray(selected) ? selected : [selected]);
}

