import "./theme.css";
import "./style.css";
import { language, setLanguagePreference, t, type LanguagePreference } from "./i18n";
import { copyText, type ActivityEntry, type LogFilter } from "./ui/activityLog";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { clarifyTransferError } from "./app/backendText";
import { appendConsoleLine, commandStorageKey, openCommandMenu, toggleSavedCommand, updateSaveCommandButton } from "./app/command";
import { connectionPayload, prepareConnection, rememberForSession, setConnectionStatus, updateAuthFields, updateAuthHintVisibility, updateConnectionSummary } from "./app/connection";
import { PAGE_ORDER } from "./app/constants";
import { closeConfirm, showConfirm } from "./app/dialogs";
import { activityLog, appVersion, authTypeSelect, blockMap, bookmarkBtn, browseRemoteBtn, cancelBtn, changeFileBtn, clearCheckpointBtn, commandMenu, commandMenuBtn, confirmCancel, confirmOk, confirmOverlay, copyLogBtn, deleteProfileBtn, editServerBtn, forceOverwriteInput, hostInput, hostSelect, keyPassphraseInput, keyPathInput, languageHint, localPathInput, miniTransfer, newServerBtn, passwordInput, pathMenu, pathMenuBtn, pickFolderBtn, portInput, postCommandInput, profileNameInput, profileSelect, rememberSecretsInput, remoteBrowseCancelNewDirBtn, remoteBrowseCloseBtn, remoteBrowseCreateDirBtn, remoteBrowseGoBtn, remoteBrowseList, remoteBrowseNewDirBtn, remoteBrowseNewDirName, remoteBrowseOverlay, remoteBrowsePath, remoteBrowseSelectDirBtn, remoteBrowseUpBtn, remotePathInput, resultBackBtn, resultCard, saveCommandBtn, saveProfileBtn, statusText, testConnBtn, transferBtn, usernameInput, viewLogBtn } from "./app/dom";
import { describeError, fileNameOf, formatBytes } from "./app/format";
import { clearCheckpointFor, loadHistory, restoreActiveCheckpoint, resumePendingTransfer } from "./app/history";
import { showPage } from "./app/nav";
import { isQueueSelection, openPathMenu, pickLocalFolders, pickLocalPaths, setupDragDrop, toggleBookmark, updateDropZoneState, updateResolvedPathHint } from "./app/paths";
import { clearProbeResult, invalidateProbeOnEdit, runBackgroundProbe, updateTransferStatePanel } from "./app/probe";
import { refreshLiveVisuals, setBadge, setProgressState, setStatus, showReconnectingSpeedText, showTransferStatus, syncMapLegend, updateProgressDetail } from "./app/progress";
import { startQueueFlow } from "./app/queue";
import { applyRemoteBrowseSelection, applyRemoteDownloadPicks, closeRemoteBrowseOverlay, createRemoteBrowseDir, hideRemoteBrowseNewDirForm, loadRemoteBrowseDir, openRemoteBrowseDialog, remoteBrowsePicked, showRemoteBrowseNewDirForm, updateRemoteBrowseModeUi } from "./app/remoteBrowse";
import { applyHostSelection, applyProfile, defaultProfileName, loadHosts, loadProfiles, startNewServer } from "./app/servers";
import { applyThemePreference, initLanguagePreference, initThemePreference } from "./app/settings";
import { state } from "./app/state";
import { getTransferredBytes, handleProgressUpdate, isDownloadMode, setTransferring, showTransferView, startDownloadFlow, startUploadFlow, updateModeHint, updateTransferDirectionUI } from "./app/transfer";
import { ConnectionProfile, DownloadCheckpoint, DownloadProgressEvent, DownloadRetryEvent, PageId, ThemePreference, TransferDirection, UploadCheckpoint, UploadProgressEvent, UploadRetryEvent } from "./app/types";

// 先告诉后端界面语言：之后后端生成的错误信息、活动日志都按这个语言输出。
void invoke("set_language", { language: language() }).catch(() => {
  // 浏览器预览模式下没有后端。
});


authTypeSelect.addEventListener("change", updateAuthFields);

rememberSecretsInput.addEventListener("change", updateAuthHintVisibility);


profileSelect.addEventListener("change", async () => {
  const id = profileSelect.value;
  if (!id) {
    updateConnectionSummary();
    return;
  }
  const profile = state.savedProfiles.find((p) => p.id === id);
  if (profile) await applyProfile(profile);
});


saveProfileBtn.addEventListener("click", async () => {
  const name = profileNameInput.value.trim() || defaultProfileName();
  if (!name) {
    setConnectionStatus(t("请先填写连接名称，或选择 SSH Config Host"), "error");
    profileNameInput.focus();
    return;
  }

  try {
    const saved = await invoke<ConnectionProfile>("save_connection_profile", {
      request: {
        name,
        host: hostInput.value.trim(),
        port: Number(portInput.value) || 22,
        username: usernameInput.value.trim(),
        auth_type: authTypeSelect.value,
        key_path: keyPathInput.value.trim() || null,
        password: passwordInput.value || null,
        key_passphrase: keyPassphraseInput.value || null,
        remember_secrets: rememberSecretsInput.checked,
      },
    });
    await loadProfiles();
    profileSelect.value = saved.id;
    profileNameInput.value = saved.name;
    rememberForSession();
    const stored = !rememberSecretsInput.checked
      ? t("，密码不保存（仅本次使用）")
      : authTypeSelect.value === "password" && passwordInput.value
        ? t("，密码已保存")
        : keyPassphraseInput.value
          ? t("，私钥口令已保存")
          : "";
    setConnectionStatus(t("已保存为「{name}」{stored}", { name: saved.name, stored }), "ok");
  } catch (error) {
    setConnectionStatus(describeError(error), "error");
  }
});


deleteProfileBtn.addEventListener("click", async () => {
  const id = profileSelect.value;
  if (!id) {
    setConnectionStatus(t("请先选择要删除的连接"), "error");
    return;
  }
  const confirmed = await showConfirm(
    t("删除连接"),
    t("确定删除这个已保存的连接？"),
  );
  if (!confirmed) return;

  try {
    await invoke("delete_connection_profile", { id });
    profileSelect.value = "";
    profileNameInput.value = "";
    await loadProfiles();
    setConnectionStatus(t("连接配置已删除"), "ok");
  } catch (error) {
    setConnectionStatus(describeError(error), "error");
  }
});


hostSelect.addEventListener("change", () => {
  void applyHostSelection(hostSelect.value);
});


[hostInput, usernameInput, portInput].forEach((input) => {
  input.addEventListener("input", () => {
    updateConnectionSummary();
    if (!profileNameInput.value.trim()) {
      profileNameInput.placeholder = defaultProfileName() || t("保存名称，如 myserver 或 user@host");
    }
  });
});


[passwordInput, keyPathInput, keyPassphraseInput].forEach((input) => {
  input.addEventListener("input", () => {
    rememberForSession();
    updateConnectionSummary();
    updateAuthHintVisibility();
  });
});


document.querySelectorAll('input[name="transferDirection"]').forEach((input) => {
  input.addEventListener("change", () => {
    const selected = document.querySelector<HTMLInputElement>(
      'input[name="transferDirection"]:checked',
    );
    state.transferDirection = (selected?.value as TransferDirection) ?? "upload";
    updateTransferDirectionUI();
    clearProbeResult();
    void restoreActiveCheckpoint();
  });
});


document.querySelectorAll('input[name="transferMode"]').forEach((input) => {
  input.addEventListener("change", updateModeHint);
});


confirmCancel.addEventListener("click", () => closeConfirm(false));

confirmOk.addEventListener("click", () => closeConfirm(true));

confirmOverlay.addEventListener("click", (event) => {
  if (event.target === confirmOverlay) closeConfirm(false);
});

document.addEventListener("keydown", (event) => {
  if (
    event.key === "Escape" &&
    confirmOverlay.classList.contains("is-visible") &&
    !state.confirmClosing
  ) {
    closeConfirm(false);
    return;
  }
  if (
    event.key === "Escape" &&
    remoteBrowseOverlay.classList.contains("is-visible")
  ) {
    closeRemoteBrowseOverlay();
  }
});


browseRemoteBtn.addEventListener("click", () => {
  void openRemoteBrowseDialog();
});


remoteBrowseCloseBtn.addEventListener("click", () => {
  closeRemoteBrowseOverlay();
});


remoteBrowseOverlay.addEventListener("click", (event) => {
  if (event.target === remoteBrowseOverlay) {
    closeRemoteBrowseOverlay();
  }
});


remoteBrowseGoBtn.addEventListener("click", () => {
  void loadRemoteBrowseDir(remoteBrowsePath.value);
});


remoteBrowsePath.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    void loadRemoteBrowseDir(remoteBrowsePath.value);
  }
});


remoteBrowseUpBtn.addEventListener("click", () => {
  if (state.remoteBrowseCurrent?.parent) {
    void loadRemoteBrowseDir(state.remoteBrowseCurrent.parent);
  }
});


remoteBrowseSelectDirBtn.addEventListener("click", () => {
  if (isDownloadMode()) {
    // 保持列表中的显示顺序，下载顺序与用户看到的一致。
    const picked = [...remoteBrowseList.querySelectorAll<HTMLElement>(".remote-browse-item.selected")]
      .map((row) => row.dataset.path ?? "")
      .filter((path) => remoteBrowsePicked.has(path));
    void applyRemoteDownloadPicks(picked);
    return;
  }
  const path = state.remoteBrowseCurrent?.path ?? remoteBrowsePath.value.trim();
  if (!path) return;
  applyRemoteBrowseSelection(path, true);
});


remoteBrowseNewDirBtn.addEventListener("click", () => {
  showRemoteBrowseNewDirForm();
});


remoteBrowseCreateDirBtn.addEventListener("click", () => {
  void createRemoteBrowseDir();
});


remoteBrowseCancelNewDirBtn.addEventListener("click", () => {
  hideRemoteBrowseNewDirForm();
});


remoteBrowseNewDirName.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    void createRemoteBrowseDir();
  }
});


testConnBtn.addEventListener("click", async () => {
  try {
    testConnBtn.disabled = true;
    setConnectionStatus(t("正在测试连接…"));
    await prepareConnection();
    const message = await invoke<string>("test_connection", {
      request: connectionPayload(),
    });
    setConnectionStatus(message, "ok");
  } catch (error) {
    setConnectionStatus(clarifyTransferError(describeError(error)), "error");
  } finally {
    if (!state.isTransferring) testConnBtn.disabled = false;
  }
});


document.querySelector("#pickKeyBtn")!.addEventListener("click", async () => {
  const selected = await open({ multiple: false });
  if (typeof selected === "string") {
    keyPathInput.value = selected;
  }
});


document.querySelector("#pickFileBtn")!.addEventListener("click", () => void pickLocalPaths());

pickFolderBtn.addEventListener("click", () => void pickLocalFolders());

changeFileBtn.addEventListener("click", () => void pickLocalPaths());


forceOverwriteInput.addEventListener("change", () => {
  if (state.lastProbe) updateTransferStatePanel();
});


document.querySelectorAll<HTMLInputElement>('input[name="languageMode"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    // 切换语言会重新加载页面；传输虽在后端继续，但进行中视图会丢失本次的续传起点等信息。
    if (state.isTransferring || state.commandRunning) {
      initLanguagePreference();
      languageHint.textContent = t("传输进行中，完成后再切换语言");
      return;
    }
    setLanguagePreference(radio.value as LanguagePreference);
  });
});


document.querySelectorAll<HTMLInputElement>('input[name="themeMode"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (!radio.checked) return;
    applyThemePreference(radio.value as ThemePreference);
  });
});


localPathInput.addEventListener("input", () => {
  updateDropZoneState();
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});


localPathInput.addEventListener("blur", () => {
  if (!state.isTransferring) void runBackgroundProbe();
});


bookmarkBtn.addEventListener("click", () => void toggleBookmark());

saveCommandBtn.addEventListener("click", () => void toggleSavedCommand());

commandMenuBtn.addEventListener("click", () => {
  if (commandMenu.isOpen) commandMenu.close();
  else openCommandMenu();
});

postCommandInput.addEventListener("input", () => {
  localStorage.setItem(commandStorageKey(), postCommandInput.value);
  updateSaveCommandButton();
});

void listen<{ line: string }>("command-output", (event) => {
  if (state.commandRunning) appendConsoleLine(event.payload.line);
});

pathMenuBtn.addEventListener("click", () => {
  if (pathMenu.isOpen) pathMenu.close();
  else openPathMenu();
});


remotePathInput.addEventListener("input", () => {
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});


remotePathInput.addEventListener("blur", () => {
  if (!state.isTransferring) void runBackgroundProbe();
});


transferBtn.addEventListener("click", async () => {
  if (state.startInFlight || state.isTransferring) return;
  state.startInFlight = true;
  transferBtn.disabled = true;
  try {
    // 先判断是否多文件：上传和下载都可能是队列，不能先按方向分流。
    if (isQueueSelection()) {
      await startQueueFlow();
    } else if (isDownloadMode()) {
      await startDownloadFlow();
    } else {
      await startUploadFlow();
    }
  } catch (error) {
    setTransferring(false);
    // 后端拒绝启动时已经切到了进行中视图，退回编辑视图让用户看到错误并修改。
    if (state.currentView === "run") showTransferView("compose");
    setStatus(describeError(error), "error");
  } finally {
    state.startInFlight = false;
    // 用户在确认框里取消时流程正常返回、没有进入传输态，需要把按钮还回去。
    if (!state.isTransferring) transferBtn.disabled = false;
  }
});



resultBackBtn.addEventListener("click", () => {
  // 成功后「再传一个」：保留服务器和目标目录，只清掉上一个文件，方便连续传多个文件。
  if (resultCard.dataset.transferOk === "1") {
    if (isDownloadMode()) remotePathInput.value = remotePathInput.value.replace(/[^/]*$/, "");
    else localPathInput.value = "";
    updateDropZoneState();
    updateResolvedPathHint();
    clearProbeResult();
  }
  statusText.textContent = "";
  showTransferView("compose");
});


editServerBtn.addEventListener("click", () => showPage("servers"));

viewLogBtn.addEventListener("click", () => showPage("logs"));


document.querySelectorAll<HTMLInputElement>('input[name="logFilter"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (radio.checked) activityLog.setFilter(radio.value as LogFilter);
  });
});


copyLogBtn.addEventListener("click", async () => {
  await copyText(activityLog.toText());
  const label = copyLogBtn.lastChild;
  if (label) label.textContent = t("已复制");
  window.setTimeout(() => {
    if (label) label.textContent = t("复制");
  }, COPY_FEEDBACK_MS);
});


document.querySelector<HTMLButtonElement>("#clearLogBtn")!.addEventListener("click", () => {
  activityLog.clear();
  void invoke("clear_activity_log").catch(() => {});
});


// 先监听再补拉：补拉期间到达的实时日志由 ActivityLogView 按 id 去重。
void listen<ActivityEntry>("activity-log", (event) => activityLog.append(event.payload));

void invoke<ActivityEntry[]>("list_activity_log")
  .then((entries) => activityLog.load(entries))
  .catch(() => {
    // 浏览器预览模式下没有后端，日志页保持空状态。
  });

newServerBtn.addEventListener("click", startNewServer);

miniTransfer.addEventListener("click", () => showPage("transfer"));


document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((item) => {
  item.addEventListener("click", () => showPage(item.dataset.page as PageId));
});


/** ⌘1–4 切页、⌘↩ 开始、⌘O 选文件：常用操作不必离开键盘。 */
document.addEventListener("keydown", (event) => {
  if (!event.metaKey || event.altKey || event.ctrlKey) return;
  if (confirmOverlay.classList.contains("is-visible") || remoteBrowseOverlay.classList.contains("is-visible")) return;
  const pageIndex = Number(event.key) - 1;
  if (Number.isInteger(pageIndex) && pageIndex >= 0 && pageIndex < PAGE_ORDER.length) {
    event.preventDefault();
    showPage(PAGE_ORDER[pageIndex]);
    return;
  }
  if (event.key === "Enter" && state.currentPage === "transfer" && state.currentView !== "run") {
    event.preventDefault();
    transferBtn.click();
    return;
  }
  if (event.key.toLowerCase() === "o" && state.currentPage === "transfer" && state.currentView === "compose") {
    event.preventDefault();
    document.querySelector<HTMLButtonElement>("#pickFileBtn")!.click();
  }
});


cancelBtn.addEventListener("click", () => {
  if (state.commandRunning) {
    cancelBtn.disabled = true;
    cancelBtn.textContent = t("停止中…");
    void invoke("cancel_remote_command").catch(() => {});
    return;
  }
  if (state.cancelInFlight || !state.isTransferring) return;

  state.cancelInFlight = true;
  cancelBtn.disabled = true;
  cancelBtn.textContent = t("取消中…");
  cancelBtn.classList.add("is-cancelling");
  setBadge(t("取消中"), "retrying");
  setStatus(t("正在取消…"));

  const cmd = state.runDirection === "download" ? "cancel_download" : "cancel_upload";
  void invoke(cmd).catch((error) => {
    state.cancelInFlight = false;
    if (state.isTransferring) {
      cancelBtn.disabled = false;
      cancelBtn.textContent = t("取消传输");
      cancelBtn.classList.remove("is-cancelling");
    }
    setStatus(describeError(error), "error");
  });
});


clearCheckpointBtn.addEventListener("click", () => {
  void clearCheckpointFor(state.transferDirection);
});



/** 「已复制」反馈停留时长。 */
const COPY_FEEDBACK_MS = 1500;


void listen("transfer-close-requested", async () => {
  const ok = await showConfirm(
    t("传输进行中"),
    t("关闭窗口将取消当前传输，已传输的部分会保留为断点。\n\n确定关闭吗？"),
  );
  if (!ok) return;

  await invoke("prepare_app_close");
  await getCurrentWindow().close();
});


/**
 * 启动顺序有依赖：恢复断点时要按 host/user 匹配已保存的服务器，所以先加载服务器列表；
 * 是否提示继续传输，又要等确认后端当前没有正在进行的任务之后才能决定。
 */
async function startupSequence() {
  const settle = (task: Promise<unknown>) => task.catch(() => {});
  await settle(loadProfiles());
  await settle(restoreActiveCheckpoint());
  await settle(syncTransferStateOnStartup());
  await offerResumeOnLaunch();
}


/**
 * 启动时发现未完成的传输就主动询问是否继续：应用崩溃或电脑重启后，
 * 用户往往不记得还有传了一半的文件，等他去历史页翻就晚了。
 */
async function offerResumeOnLaunch() {
  if (state.isTransferring) return;
  const pending: [TransferDirection, UploadCheckpoint | DownloadCheckpoint][] = [];
  if (state.activeUploadCheckpoint) pending.push(["upload", state.activeUploadCheckpoint]);
  if (state.activeDownloadCheckpoint) pending.push(["download", state.activeDownloadCheckpoint]);
  if (pending.length === 0) return;

  const [direction, cp] = pending[0];
  const transferred = getTransferredBytes(cp);
  const name = fileNameOf(direction === "download" ? cp.remote_path : cp.local_path);
  const percent = cp.file_size > 0 ? Math.floor((transferred / cp.file_size) * 100) : 0;
  const more = pending.length > 1 ? t("\n\n另有 1 个未完成的传输，可在「历史」页查看。") : "";
  const resume = await showConfirm(
    t("有未完成的传输"),
    t("「{name}」已{verb} {percent}%（{done} / {total}），要现在继续吗？", {
      name,
      verb: direction === "download" ? t("下载") : t("上传"),
      percent,
      done: formatBytes(transferred),
      total: formatBytes(cp.file_size),
    }) + more,
    { ok: t("继续传输"), cancel: t("稍后") },
  );
  if (!resume) return;
  await resumePendingTransfer(direction);
  transferBtn.click();
}


async function syncTransferStateOnStartup() {
  try {
    const running = await invoke<{ upload_running: boolean; download_running: boolean }>(
      "get_transfer_running",
    );
    if (running.upload_running || running.download_running) {
      state.transferDirection = running.download_running ? "download" : "upload";
      const radio = document.querySelector<HTMLInputElement>(
        `input[name="transferDirection"][value="${state.transferDirection}"]`,
      );
      if (radio) radio.checked = true;
      updateTransferDirectionUI();
      setTransferring(true);
      // 重新接上已在进行的传输：真实起点未知，不能把此刻的进度画成「之前已续传」。
      blockMap.reset(0, 0);
      syncMapLegend();
      setProgressState("uploading");
      setBadge(running.download_running ? t("下载中") : t("上传中"), "uploading");
      setStatus(t("检测到进行中的传输任务…"));
    }
  } catch {
    // 忽略启动同步失败
  }
}


void listen<UploadProgressEvent>("upload-progress", (event) => {
  const payload = event.payload;
  handleProgressUpdate(
    payload.uploaded_bytes,
    payload.total_bytes,
    payload.status,
    payload.message,
    payload.retry_count,
    "upload",
    payload.verify_summary,
  );
});


void listen<DownloadProgressEvent>("download-progress", (event) => {
  const payload = event.payload;
  handleProgressUpdate(
    payload.downloaded_bytes,
    payload.total_bytes,
    payload.status,
    payload.message,
    payload.retry_count,
    "download",
    payload.verify_summary,
  );
});


void listen<UploadRetryEvent>("upload-retry", (event) => {
  state.retryCount = event.payload.retry_count;
  state.retryDelayMs = event.payload.delay_ms;
  state.retryScheduledAt = Date.now();
  state.isReconnecting = true;
  state.isConnecting = false;
  blockMap.markRetry();
  syncMapLegend();
  updateProgressDetail(state.progressTransferredBytes, state.progressTotalBytes, state.retryCount);
  setProgressState("retrying");
  setBadge(t("重试中"), "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  refreshLiveVisuals();
});


void listen<DownloadRetryEvent>("download-retry", (event) => {
  state.retryCount = event.payload.retry_count;
  state.retryDelayMs = event.payload.delay_ms;
  state.retryScheduledAt = Date.now();
  state.isReconnecting = true;
  state.isConnecting = false;
  blockMap.markRetry();
  syncMapLegend();
  updateProgressDetail(state.progressTransferredBytes, state.progressTotalBytes, state.retryCount);
  setProgressState("retrying");
  setBadge(t("重试中"), "retrying");
  showReconnectingSpeedText();
  showTransferStatus(event.payload.message, "retry");
  refreshLiveVisuals();
});


setupDragDrop();

initThemePreference();

initLanguagePreference();

showTransferView("compose");

updateConnectionSummary();

updateModeHint();

updateTransferDirectionUI();

void loadHosts();

void loadHistory();

void startupSequence();

void getVersion()
  .then((version) => {
    appVersion.textContent = `v${version}`;
  })
  .catch(() => {
    // 浏览器预览模式下拿不到版本号，留空即可。
  });

updateAuthFields();

updateDropZoneState();

updateResolvedPathHint();

updateAuthHintVisibility();

updateRemoteBrowseModeUi();

