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
import { connectionPayload, forgetJumpSecrets, prepareConnection, rememberForSession, setConnectionStatus, updateAuthFields, updateAuthHintVisibility, updateConnectionSummary } from "./app/connection";
import { COPY_FEEDBACK_MS, DEFAULT_SSH_PORT, PAGE_ORDER } from "./app/constants";
import { closeConfirm, showConfirm } from "./app/dialogs";
import { activityLog, appVersion, authTypeSelect, blockMap, bookmarkBtn, browseRemoteBtn, cancelBtn, changeFileBtn, clearCheckpointBtn, commandMenu, commandMenuBtn, confirmCancel, confirmOk, confirmOverlay, copyLogBtn, copyReceiptBtn, deleteProfileBtn, editServerBtn, forceOverwriteInput, hostInput, hostSelect, jumpProfileSelect, keyPassphraseInput, keyPathInput, languageHint, localPathInput, miniTransfer, newServerBtn, passwordInput, pathMenu, pathMenuBtn, pickFolderBtn, portInput, postCommandInput, profileNameInput, profileSelect, rememberSecretsInput, remoteBrowseCancelNewDirBtn, remoteBrowseCloseBtn, remoteBrowseCreateDirBtn, remoteBrowseGoBtn, remoteBrowseList, remoteBrowseNewDirBtn, remoteBrowseNewDirName, remoteBrowseOverlay, remoteBrowsePath, remoteBrowseSelectDirBtn, remoteBrowseUpBtn, remotePathInput, resultBackBtn, resultCard, saveCommandBtn, saveProfileBtn, saveReceiptBtn, statusText, testConnBtn, transferBtn, usernameInput, viewLogBtn } from "./app/dom";
import { describeError, fileNameOf, floorPercent, formatBytes } from "./app/format";
import { clearCheckpointFor, loadHistory, refreshCheckpointFromDisk, restoreActiveCheckpoint, resumePendingTransfer } from "./app/history";
import { showPage } from "./app/nav";
import { isQueueSelection, openPathMenu, pickLocalFolders, pickLocalPaths, setupDragDrop, toggleBookmark, updateDropZoneState, updateResolvedPathHint } from "./app/paths";
import { clearLastProbe, clearProbeResult, invalidateProbeOnEdit, runBackgroundProbe, updateTransferStatePanel } from "./app/probe";
import { refreshLiveVisuals, setBadge, setProgressState, setStatus, showReconnectingSpeedText, showTransferStatus, syncMapLegend, updateProgressDetail } from "./app/progress";
import { offerSavedBatchOnLaunch, startQueueFlow } from "./app/queue";
import { clearSavedBatch } from "./app/savedBatch";
import { applyRemoteBrowseSelection, applyRemoteDownloadPicks, closeRemoteBrowseOverlay, createRemoteBrowseDir, hideRemoteBrowseNewDirForm, loadRemoteBrowseDir, openRemoteBrowseDialog, remoteBrowsePicked, showRemoteBrowseNewDirForm, updateRemoteBrowseModeUi } from "./app/remoteBrowse";
import { applyHostSelection, applyProfile, defaultProfileName, loadHosts, loadProfiles, setJumpSelection, startNewServer } from "./app/servers";
import { applyParallelConnections, applyThemePreference, initLanguagePreference, initParallelConnections, initThemePreference } from "./app/settings";
import { copyReceipt, saveReceipt } from "./app/receipt";
import { isBusy, state } from "./app/state";
import { clearStartPhase, getTransferredBytes, handleProgressUpdate, isDownloadMode, setTransferring, showStartPhase, showTransferView, startDownloadFlow, startUploadFlow, syncBusyLock, updateModeHint, updateTransferButtonLabel, updateTransferDirectionUI } from "./app/transfer";
import { ConnectionProfile, DownloadCheckpoint, DownloadProgressEvent, DownloadRetryEvent, PageId, ThemePreference, TransferDirection, UploadCheckpoint, UploadProgressEvent, UploadRetryEvent } from "./app/types";

// 先告诉后端界面语言：之后后端生成的错误信息、活动日志都按这个语言输出。
void invoke("set_language", { language: language() }).catch(() => {
  // 浏览器预览模式下没有后端。
});


authTypeSelect.addEventListener("change", updateAuthFields);

rememberSecretsInput.addEventListener("change", updateAuthHintVisibility);


jumpProfileSelect.addEventListener("change", () => updateConnectionSummary());
copyReceiptBtn.addEventListener("click", () => void copyReceipt());
// 设置页的路径行：点击（或回车/空格）在访达中显示，方便反馈问题时找到日志。
document.querySelectorAll<HTMLElement>("[data-reveal]").forEach((row) => {
  const reveal = () =>
    invoke("reveal_app_location", { location: row.dataset.reveal }).catch((error) =>
      console.warn("reveal failed", error),
    );
  row.addEventListener("click", reveal);
  row.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      void reveal();
    }
  });
});
saveReceiptBtn.addEventListener("click", () => void saveReceipt());


profileSelect.addEventListener("change", async () => {
  const id = profileSelect.value;
  if (!id) {
    setJumpSelection(null);
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
        // 带上正在编辑的服务器 id：后端据此更新这一台（改名也保留收藏和常用命令）；
        // 为 null 表示新建，重名时后端会报错而不是悄悄覆盖同名的另一台。
        id: profileSelect.value || null,
        name,
        host: hostInput.value.trim(),
        port: Number(portInput.value) || DEFAULT_SSH_PORT,
        username: usernameInput.value.trim(),
        auth_type: authTypeSelect.value,
        key_path: keyPathInput.value.trim() || null,
        password: passwordInput.value || null,
        key_passphrase: keyPassphraseInput.value || null,
        remember_secrets: rememberSecretsInput.checked,
        jump_profile_id: jumpProfileSelect.value || null,
      },
    });
    await loadProfiles();
    profileSelect.value = saved.id;
    profileNameInput.value = saved.name;
    setJumpSelection(saved.jump_profile_id);
    // 这台服务器可能是别的服务器的跳板机，密码改过后要重新读取。
    forgetJumpSecrets(saved.id);
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
    // 预检结论和断点是否匹配都取决于服务器地址，改了就要重新判断。
    clearLastProbe();
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
    // 只刷新断点状态，不回填表单：切方向时用断点里的服务器和路径覆盖用户正在填的内容，
    // 是把「看一眼另一个方向」变成了「丢掉当前输入」。回填只在启动时和点「继续」时做。
    void refreshCheckpointFromDisk().catch((error) => console.warn("failed to refresh checkpoint", error));
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
    if (!isBusy()) testConnBtn.disabled = false;
  }
});


document.querySelector("#pickKeyBtn")!.addEventListener("click", async () => {
  const selected = await open({ multiple: false });
  if (typeof selected === "string") {
    keyPathInput.value = selected;
    // 直接赋值不会触发 input 事件，手动派发一次，让连接摘要和本次会话记住的凭据跟着更新。
    keyPathInput.dispatchEvent(new Event("input"));
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
    // 切换语言会重新加载页面；传输虽在后端继续，但进行中视图会丢失本次的续传起点等信息，
    // 批量还会丢掉尚未开始的文件。提示在忙碌结束时由 syncBusyLock 清除。
    if (isBusy()) {
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


document.querySelectorAll<HTMLInputElement>('input[name="parallelConnections"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    if (radio.checked) applyParallelConnections(Number(radio.value));
  });
});


localPathInput.addEventListener("input", () => {
  updateDropZoneState();
  updateResolvedPathHint();
  invalidateProbeOnEdit();
});


// 是否该预检（忙碌、表单没变等）由 runBackgroundProbe 自己判断。
localPathInput.addEventListener("blur", () => void runBackgroundProbe());


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


remotePathInput.addEventListener("blur", () => void runBackgroundProbe());


transferBtn.addEventListener("click", async () => {
  if (isBusy()) return;
  state.startInFlight = true;
  syncBusyLock();
  // 从这一刻到进入进行中视图之间要确认主机指纹、做一次完整的预检连接，弱网下可达数秒：
  // 立刻换掉按钮文案并在页脚说明进行到哪一步，否则看起来像点了没反应。
  transferBtn.textContent = t("准备中…");
  showStartPhase(t("正在连接服务器…"));
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
    // 所有出口（成功进入传输、确认框里取消、出错）都走这里：撤掉阶段提示、还原按钮文案，
    // 并按最终状态决定锁不锁——已进入传输态时保持锁定，否则把表单和按钮还回去。
    clearStartPhase();
    updateTransferButtonLabel();
    syncBusyLock();
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
  } else {
    // 在没传完的结果页上选择「返回修改」而不是「重试 / 继续传输」：用户不打算接着传这个批量了，
    // 下次启动不再提示。剩余文件仍留在选择里，再点开始会重新保存。
    clearSavedBatch();
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
  const label = copyLogBtn.lastChild;
  let feedback = t("已复制");
  try {
    await copyText(activityLog.toText());
  } catch (error) {
    // 剪贴板写入可能被系统拒绝；日志页没有状态栏，就在按钮上如实说明没复制成功。
    console.warn("copy log failed", error);
    feedback = t("复制失败");
  }
  if (label) label.textContent = feedback;
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


/** ⌘1–5 切页、⌘↩ 开始、⌘O 选文件：常用操作不必离开键盘。 */
document.addEventListener("keydown", (event) => {
  if (!event.metaKey || event.altKey || event.ctrlKey) return;
  if (confirmOverlay.classList.contains("is-visible") || remoteBrowseOverlay.classList.contains("is-visible")) return;
  const pageIndex = Number(event.key) - 1;
  if (Number.isInteger(pageIndex) && pageIndex >= 0 && pageIndex < PAGE_ORDER.length) {
    event.preventDefault();
    showPage(PAGE_ORDER[pageIndex]);
    return;
  }
  // 成功结果页上开始按钮只是被隐藏：不排除的话 ⌘↩ 会把刚完成的传输再跑一遍，连带重复执行传完后命令。
  const startVisible = !transferBtn.classList.contains("hidden");
  if (event.key === "Enter" && state.currentPage === "transfer" && state.currentView !== "run" && startVisible) {
    event.preventDefault();
    transferBtn.click();
    return;
  }
  if (event.key.toLowerCase() === "o" && state.currentPage === "transfer" && state.currentView === "compose" && !isBusy()) {
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
  if (state.cancelInFlight) return;
  if (!state.isTransferring && !state.activeQueue) return;

  cancelBtn.disabled = true;
  cancelBtn.textContent = t("取消中…");
  setBadge(t("取消中"), "retrying");
  // 批量：无论当前文件能否被取消，都不再开始后面的文件。也覆盖「取消发出时当前文件刚好传完」的情况。
  if (state.activeQueue) state.queueCancelRequested = true;
  // 批量的文件间隙（上一个已结束、下一个还在预检）没有后端任务可取消，置好标记即可，
  // 队列会在启动下一个文件前停下。
  if (!state.isTransferring) return;

  state.cancelInFlight = true;
  setStatus(t("正在取消…"));

  const cmd = state.runDirection === "download" ? "cancel_download" : "cancel_upload";
  void invoke(cmd).catch((error) => {
    state.cancelInFlight = false;
    // 取消没发出去：当前文件仍在传，批量也照常继续，把按钮还给用户重试。
    state.queueCancelRequested = false;
    if (state.isTransferring) {
      cancelBtn.disabled = false;
      cancelBtn.textContent = t("取消传输");
    }
    setStatus(describeError(error), "error");
  });
});


clearCheckpointBtn.addEventListener("click", () => {
  void clearCheckpointFor(state.transferDirection);
});


void listen("transfer-close-requested", async () => {
  const ok = await showConfirm(
    t("传输进行中"),
    t("关闭窗口将取消当前传输，已传输的部分会保留为断点。\n\n确定关闭吗？"),
  );
  if (!ok) return;

  try {
    await invoke("prepare_app_close");
    await getCurrentWindow().close();
  } catch (error) {
    // 事件回调里的拒绝没有人接：不处理的话用户点了「确定」却什么都没发生，也不知道为什么。
    // 传输中页脚状态行是隐藏的，用 showTransferStatus 让它重新可见。
    console.warn("close after confirm failed", error);
    showTransferStatus(describeError(error), "error");
  }
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
  if (isBusy()) return;
  // 批量优先：它包含了那个传到一半的文件，问过批量就不必再为单个文件问一遍。
  if (await offerSavedBatchOnLaunch()) return;
  const pending: [TransferDirection, UploadCheckpoint | DownloadCheckpoint][] = [];
  if (state.activeUploadCheckpoint) pending.push(["upload", state.activeUploadCheckpoint]);
  if (state.activeDownloadCheckpoint) pending.push(["download", state.activeDownloadCheckpoint]);
  if (pending.length === 0) return;

  const [direction, cp] = pending[0];
  const transferred = getTransferredBytes(cp);
  const name = fileNameOf(direction === "download" ? cp.remote_path : cp.local_path);
  const percent = floorPercent(transferred, cp.file_size);
  const more = pending.length > 1 ? t("\n\n另有 1 个未完成的传输，可在「历史」页查看。") : "";
  const resume = await showConfirm(
    t("有未完成的传输"),
    t("「{name}」已{verb} {percent}%（{done} / {total}）。\n要现在继续吗？", {
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
    payload.done_ranges,
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
    payload.done_ranges,
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

initParallelConnections();

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

