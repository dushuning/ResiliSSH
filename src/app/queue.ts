// 批量传输：把多个文件逐个交给单文件流程执行，并汇总结果。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { clarifyTransferError } from "./backendText";
import { showCommandResult, updateSaveCommandButton } from "./command";
import { connectionPayload, prepareConnection } from "./connection";
import { DEFAULT_SSH_PORT } from "./constants";
import { showConfirm, showConfirmChoice } from "./dialogs";
import { forceOverwriteInput, hostInput, localPathInput, portInput, postCommandInput, profileSelect, queueChip, remotePathInput, resultFiles, transferBtn, usernameInput } from "./dom";
import { asDir, describeError, fileNameOf } from "./format";
import { showPage } from "./nav";
import { setSelectedDownloads, setSelectedUploads, updateResolvedPathHint } from "./paths";
import { clearDockProgress } from "./dock";
import { loadHistory, refreshCheckpointFromDisk } from "./history";
import { beginRunVisuals, hideProgressSpeed, hideVerifySummary, setBadge, setStatus, showProgressSpeed, updateQueueChip } from "./progress";
import { LoadedBatch, SavedFileStatus, clearSavedBatch, loadSavedBatch, saveBatch, saveBatchStatuses } from "./savedBatch";
import { applyProfile, currentProfile, profileById } from "./servers";
import { isBusy, state } from "./state";
import { clearStartPhase, finishSuccessfulRun, isReliableMode, resetCancelButton, setTransferDirection, setTransferring, showResult, showStartPhase, showTransferView, startOneDownload, startOneUpload, syncBusyLock, updateModeHint, verificationModeText } from "./transfer";
import { ConnectionProfile, LocalEntry, QueueItem, QueueItemStatus, RunStats, RunStepResult, TransferDirection, UploadStart } from "./types";

/**
 * 多文件传输：逐个走单文件流程。某个文件失败不影响后面的文件；用户取消则整个队列停止。
 * 传完后命令只在队列结束后执行一次——部署场景下应当全部传完再重启服务。
 */
export async function startQueueFlow() {
  await prepareConnection();
  const direction = state.transferDirection;
  // 整批都是「已相同、跳过」时不会经过 setTransferring，凭证里的方向要在这里就定下来。
  state.runDirection = direction;
  const download = direction === "download";
  // 多个文件必须放进目录，否则会互相覆盖成同一个文件。
  const baseInput = download ? localPathInput : remotePathInput;
  if (!baseInput.value.trim()) {
    throw new Error(download ? t("请填写本地保存路径") : t("请填写远端目录"));
  }
  const baseDir = asDir(baseInput.value);
  baseInput.value = baseDir;
  // 上传要连服务器逐个查同名文件，弱网下可达数秒；下载查的是本地磁盘，瞬间完成，不必提示。
  if (!download) showStartPhase(t("正在检查远端文件…"));

  const queueStartedAt = Date.now();
  // 传完后命令只用于上传（把文件送到服务器之后再做点什么）；下载时该字段是隐藏的，不能悄悄执行。
  const template = download ? "" : postCommandInput.value.trim();
  const sources = download ? [...state.selectedDownloads] : [...state.selectedUploads];
  const relDirs = new Map(download ? state.downloadRelDirs : state.uploadRelDirs);
  const roots = [...(download ? state.downloadRoots : state.uploadRoots)];
  const sizes = new Map(download ? state.downloadSizes : state.uploadSizes);
  const queue = {
    items: sources.map<QueueItem>((source) => {
      const rel = relDirs.get(source) ?? "";
      return {
        source,
        target: rel ? `${baseDir}${rel}/` : baseDir,
        label: rel ? `${rel}/${fileNameOf(source)}` : fileNameOf(source),
        remote: "",
        local: "",
        status: "pending",
      };
    }),
    index: 0,
    totalBytes: sources.every((source) => sizes.has(source))
      ? sources.reduce((sum, source) => sum + (sizes.get(source) ?? 0), 0)
      : 0,
    doneBytes: 0,
  };
  if (!(await confirmBatch(queue.items, download))) {
    setStatus(download ? t("已取消批量下载") : t("已取消批量上传"));
    return;
  }
  // 用户确认要传了才保存：应用中途退出（崩溃、关机、关窗口）后，下次启动能接着传剩下的文件。
  const persisted = persistBatchStart(direction, baseDir, roots, template, sources.map((path) => ({
    path,
    rel_dir: relDirs.get(path) ?? "",
    size: sizes.get(path) ?? 0,
  })));
  const persistProgress = () => {
    if (persisted) saveBatchStatuses(queue.items.map(savedStatusOf));
  };
  // 设置后，逐个文件的预检不再弹确认框（见 confirmUploadAction / confirmDownloadAction）。
  state.activeQueue = queue;
  state.queueCancelRequested = false;
  // 整个批量期间表单保持锁定并停在进行中视图：否则每个文件之间表单会被放开，
  // 而「全部已是最新」的批量根本不会进入传输态，用户既看不到进度也无法取消。
  syncBusyLock();
  beginRunVisuals();
  showTransferView("run");

  /** 用户取消了批量：从第 from 个起都没有开始。 */
  const markNotStarted = (from: number) => {
    for (const rest of queue.items.slice(from)) {
      rest.status = "cancelled";
      rest.message = t("未开始");
    }
  };

  try {
    for (let i = 0; i < queue.items.length; i++) {
      const item = queue.items[i];
      // 文件之间没有后端任务可取消，取消按钮只是置了标记，由这里在启动下一个文件前兑现。
      if (state.queueCancelRequested) {
        markNotStarted(i);
        break;
      }
      // 上一个文件已结束（成功、跳过或失败都算），计入总进度；放在循环开头，continue 的分支也不会漏算。
      if (i > 0) {
        queue.doneBytes += sizes.get(queue.items[i - 1].source) ?? 0;
        // 同样放在循环开头：上一个文件无论以哪种方式结束，结果都会在下一个开始前落盘。
        persistProgress();
      }
      queue.index = i;
      // 预检期间沿用上一个文件的画面，但要说明现在在做什么、轮到第几个。
      updateQueueChip(null);
      setBadge(t("连接中"), "retrying");
      showProgressSpeed(t("正在检查远端文件…"));
      if (download) {
        remotePathInput.value = item.source;
        localPathInput.value = item.target;
      } else {
        localPathInput.value = item.source;
        remotePathInput.value = item.target;
      }
      item.status = "running";
      const step = new Promise<RunStepResult>((resolve) => {
        state.resolveQueueStep = resolve;
      });
      let started: UploadStart;
      try {
        started = download ? await startOneDownload() : await startOneUpload();
      } catch (error) {
        state.resolveQueueStep = null;
        setTransferring(false);
        item.status = "failed";
        item.message = clarifyTransferError(describeError(error));
        continue;
      }
      if (started === "declined") {
        // 批量中不逐个确认，「没有启动」只可能是用户在这个文件预检期间取消了批量。
        state.resolveQueueStep = null;
        markNotStarted(i);
        break;
      }
      if (started === "identical") {
        // 两端完全一致视为已送达：计入成功，队列结束后的命令照常执行。
        state.resolveQueueStep = null;
        item.status = "done";
        item.remote = state.runRemotePath;
        item.local = state.runLocalPath;
        item.identical = true;
        item.message = t("已是最新，跳过");
        continue;
      }
      item.remote = state.runRemotePath;
      item.local = state.runLocalPath;
      const result = await step;
      item.stats = result.stats;
      if (result.status === "completed") {
        item.status = "done";
      } else if (result.status === "failed") {
        item.status = "failed";
        item.message = clarifyTransferError(result.message);
      } else {
        item.status = "cancelled";
        item.message = t("已取消，断点已保留");
        markNotStarted(i + 1);
        break;
      }
    }
  } finally {
    state.resolveQueueStep = null;
    state.activeQueue = null;
    state.queueCancelRequested = false;
    // 最后一个文件的结果，以及取消时「哪些还没传」，都要在这里补记。
    persistProgress();
    // 文件间隙里点的取消不经过 setTransferring，按钮的「取消中…」要在这里复位。
    resetCancelButton();
    // 最后一个文件若是「已相同、跳过」，预检提示还留在进行中视图里；之后执行传完后命令时不该再显示它。
    hideProgressSpeed();
    clearDockProgress();
    queueChip.classList.add("hidden");
    baseInput.value = baseDir;
  }
  await finishQueue(queue.items, template, queueStartedAt, direction, { relDirs, sizes, roots, baseDir });
}


/** 持久化只区分三种状态：没成功也没失败的（未开始、正在传、被取消）下次都要再传。 */
function savedStatusOf(item: QueueItem): SavedFileStatus {
  if (item.status === "done") return "done";
  return item.status === "failed" ? "failed" : "pending";
}


/** 保存的批量里还没成功的文件（失败的也算：继续时会重试它们）。 */
function unfinishedFiles(batch: LoadedBatch): LocalEntry[] {
  return batch.files.filter((_, index) => batch.statuses[index] !== "done");
}


/**
 * 批量开始时保存它的描述，返回是否存上了（之后才需要逐个文件更新状态）。
 * 只有选了已保存的服务器才保存：恢复时要靠它从钥匙串取凭据，而密码和口令绝不写进 localStorage，
 * 未保存的服务器下次启动时没有凭据可用。
 */
function persistBatchStart(
  direction: TransferDirection,
  baseDir: string,
  roots: string[],
  postCommand: string,
  files: LocalEntry[],
): boolean {
  const profile = currentProfile();
  if (!profile) {
    clearSavedBatch();
    return false;
  }
  const { host, port, username } = connectionPayload();
  // 「重试失败的文件」「继续传输」以及启动时的恢复，传的都是上一轮剩下的文件：
  // 把之前已完成的个数带上，提示里的「共多少个」才是用户当初选的总数。
  const prior = loadSavedBatch();
  let completedBefore = 0;
  if (prior && prior.direction === direction && prior.server.profileId === profile.id && prior.baseDir === baseDir) {
    const remaining = new Set(unfinishedFiles(prior).map((file) => file.path));
    if (files.every((file) => remaining.has(file.path))) {
      completedBefore = prior.completedBefore + prior.files.length - remaining.size;
    }
  }
  return saveBatch(
    {
      direction,
      server: { profileId: profile.id, host, port, username },
      baseDir,
      roots,
      postCommand,
      reliable: isReliableMode(),
      files,
      completedBefore,
    },
    files.map(() => "pending"),
  );
}


/**
 * 保存的批量现在还能不能继续；不能时返回给用户看的原因。
 * 服务器被删了或改成了另一台，或要上传的本地文件没了，继续下去只会得到一串看不出原因的失败。
 */
async function savedBatchBlocker(batch: LoadedBatch, profile: ConnectionProfile | undefined): Promise<string | null> {
  const { host, port, username } = batch.server;
  if (!profile || profile.host !== host || profile.port !== port || profile.username !== username) {
    return t("上次未完成的批量传输无法继续：当时使用的服务器已被删除或修改，已放弃。");
  }
  if (batch.direction === "download") return null;
  try {
    const flags = await invoke<boolean[]>("local_paths_exist", { paths: unfinishedFiles(batch).map((file) => file.path) });
    const missing = flags.filter((exists) => !exists).length;
    if (missing > 0) {
      return t("上次未完成的批量上传无法继续：有 {missing} 个本地文件已不存在，已放弃。", { missing });
    }
  } catch (error) {
    // 查不了就不拦：缺的文件会在传输时逐个报出来，总比因为一次检查失败就丢掉整个批量好。
    console.warn("saved batch source check failed", error);
  }
  return null;
}


/**
 * 把保存的批量还原成「用户刚选好文件、还没点开始」的样子：方向、服务器、模式、命令、目标目录，
 * 以及只含剩余文件的选择（带相对目录，目录结构才能还原）。之后点开始走的就是普通的批量流程，
 * 传到一半的那个文件由预检发现后端断点并续传——这里不另写一套调度。
 */
async function restoreSavedBatch(batch: LoadedBatch, profile: ConnectionProfile) {
  const download = batch.direction === "download";
  setTransferDirection(batch.direction);
  profileSelect.value = profile.id;
  await applyProfile(profile);
  const mode = document.querySelector<HTMLInputElement>(
    `input[name="transferMode"][value="${batch.reliable ? "reliable" : "fast"}"]`,
  );
  if (mode) mode.checked = true;
  updateModeHint();
  // 强制覆盖不恢复：它会让传到一半的文件从头再来，与「继续」的本意相反。
  forceOverwriteInput.checked = false;
  // 必须在 applyProfile 之后：它会按服务器回填上次的命令，而这里要的是这个批量开始时锁定的那一条。
  postCommandInput.value = batch.postCommand;
  updateSaveCommandButton();

  const remaining = unfinishedFiles(batch);
  const paths = remaining.map((file) => file.path);
  if (download) {
    // 先填保存目录：setSelectedDownloads 会据此判断是否需要补上结尾的 /。
    localPathInput.value = batch.baseDir;
    setSelectedDownloads(paths, remaining);
    state.downloadRoots = batch.roots;
  } else {
    setSelectedUploads(paths, remaining);
    remotePathInput.value = batch.baseDir;
    state.uploadRoots = batch.roots;
  }
  updateResolvedPathHint();
  showTransferView("compose");
  showPage("transfer");
}


/**
 * 启动时发现上次没传完的批量就询问是否继续。返回 true 表示已经问过用户（无论答案），
 * 调用方不必再为其中那个传到一半的文件单独问一遍。
 */
export async function offerSavedBatchOnLaunch(): Promise<boolean> {
  if (isBusy()) return false;
  const batch = loadSavedBatch();
  if (!batch) return false;
  const remaining = unfinishedFiles(batch).length;
  if (remaining === 0) {
    clearSavedBatch();
    return false;
  }
  const profile = profileById(batch.server.profileId);
  const blocker = await savedBatchBlocker(batch, profile);
  if (blocker || !profile) {
    console.warn("saved batch cannot be continued", { direction: batch.direction, remaining, reason: blocker });
    clearSavedBatch();
    if (blocker) setStatus(blocker, "retry");
    return false;
  }

  const counts = { remaining, total: batch.completedBefore + batch.files.length };
  const choice = await showConfirmChoice(
    t("有未完成的批量传输"),
    (batch.direction === "download"
      ? t("上次的批量下载还有 {remaining} 个文件未完成（共 {total} 个），继续吗？", counts)
      : t("上次的批量上传还有 {remaining} 个文件未完成（共 {total} 个），继续吗？", counts)) +
      "\n\n" +
      t("已完成的文件不会重传，传到一半的文件从断点继续。目标：{target}", {
        target: batch.direction === "download" ? batch.baseDir : `${profile.name}:${batch.baseDir}`,
      }),
    { ok: t("继续传输"), cancel: t("放弃") },
  );
  // 没作答就关掉（Esc / 点遮罩）不等于放弃：保留记录，下次启动再问。只有明确点了「放弃」才丢掉。
  if (choice === "dismissed") return true;
  if (choice === "cancel") {
    console.info("saved batch discarded by user", { direction: batch.direction, remaining });
    clearSavedBatch();
    return true;
  }
  await restoreSavedBatch(batch, profile);
  transferBtn.click();
  return true;
}


/**
 * 批量开始前只确认一次：重新上传整个 dist/ 时，若逐个文件询问「是否替换」，几百个文件就要点几百次。
 * 先统计目标端已有多少同名文件，让用户一次看清会发生什么；全是新文件时不打扰。
 */
async function confirmBatch(items: QueueItem[], download: boolean): Promise<boolean> {
  const targets = items.map((item) => `${item.target}${fileNameOf(item.source)}`);
  let existing: number | null = null;
  try {
    const flags = download
      ? await invoke<boolean[]>("local_paths_exist", { paths: targets })
      : await invoke<boolean[]>("remote_paths_exist", { request: { ...connectionPayload(), paths: targets } });
    existing = flags.filter(Boolean).length;
  } catch (error) {
    // 统计失败不影响传输本身，退回到不带数字的笼统确认。
    console.warn("batch existence check failed", error);
  }
  // 检查结束：接下来要么弹确认框、要么直接进入进行中视图，「正在检查」不该留在页脚。
  clearStartPhase();
  const force = forceOverwriteInput.checked;
  if (existing === 0 && !force) return true;

  const total = items.length;
  const fresh = existing === null ? 0 : total - existing;
  let body: string;
  if (existing === null) {
    body = download
      ? t("将下载 {total} 个文件。本地已有的同名文件会在新文件校验通过后被替换。", { total })
      : t("将上传 {total} 个文件。远端已有的同名文件会在新文件校验通过后被替换。", { total });
  } else {
    body = download
      ? t("将下载 {total} 个文件：新建 {fresh} 个，替换本地已有的 {existing} 个（新文件校验通过后才替换，此前旧文件保持不变）。", { total, fresh, existing })
      : t("将上传 {total} 个文件：新建 {fresh} 个，替换远端已有的 {existing} 个（新文件校验通过后才替换，此前旧文件保持不变）。", { total, fresh, existing });
  }
  body += "\n\n" + (force
    ? t("已勾选「强制从头覆盖」：所有文件都会从头重新传输，不续传。")
    : t("与目标端完全相同的文件会自动跳过。"));
  return showConfirm(download ? t("批量下载") : t("批量上传"), body, { ok: t("开始") });
}


/**
 * 整批只发一条系统通知（逐个文件发会把通知中心刷屏）。与单文件一致：用户主动取消时不发。
 */
function notifyBatchResult(items: QueueItem[], direction: TransferDirection, cancelled: boolean) {
  if (cancelled) return;
  const download = direction === "download";
  const total = items.length;
  const done = items.filter((i) => i.status === "done").length;
  const failed = items.filter((i) => i.status === "failed").length;
  const allDone = done === total;
  const title = allDone
    ? download ? t("{total} 个文件全部下载完成", { total }) : t("{total} 个文件全部上传完成", { total })
    : download ? t("批量下载未全部成功") : t("批量上传未全部成功");
  const body = !allDone
    ? t("成功 {done} 个，失败 {failed} 个", { done, failed })
    : download
      ? t("已保存到 {dir}", { dir: localPathInput.value.trim() })
      : t("已传到 {p1}:{p2}", { p1: hostInput.value.trim(), p2: remotePathInput.value.trim() });
  invoke("notify_batch_result", { title: `ResiliSSH · ${title}`, body }).catch((error) =>
    console.warn("batch notification failed", error),
  );
}


/** 批量的展示名：来自同一个文件夹时以文件夹为主（如「dist/」），否则写文件数。 */
function batchLabel(items: QueueItem[], relDirs: Map<string, string>): string {
  const tops = new Set(items.map((i) => (relDirs.get(i.source) ?? "").split("/")[0]));
  if (tops.size === 1 && !tops.has("")) return `${[...tops][0]}/`;
  return t("{count} 个文件", { count: items.length });
}


/**
 * 批量整体记一条历史（含取消、部分失败）。逐个文件记录会让一次几百个文件的批量把历史全部挤掉。
 * 写历史失败不影响结果页，只记控制台。
 */
async function recordBatchHistory(
  items: QueueItem[],
  direction: TransferDirection,
  selection: { relDirs: Map<string, string>; sizes: Map<string, number>; roots: string[]; baseDir: string },
  stats: RunStats,
  cancelled: boolean,
) {
  const download = direction === "download";
  const done = items.filter((i) => i.status === "done");
  const failed = items.filter((i) => i.status === "failed");
  const status = done.length === items.length ? "completed" : cancelled ? "cancelled" : "failed";
  const firstRoot = selection.roots[0] ?? "";
  try {
    await invoke("record_batch_history", {
      entry: {
        id: "",
        // 上传：本地是当初选择的路径、远端是目标目录；下载反之。复用时据此回填。
        local_path: download ? selection.baseDir : firstRoot,
        remote_path: download ? firstRoot : selection.baseDir,
        host: hostInput.value.trim(),
        port: Number(portInput.value) || DEFAULT_SSH_PORT,
        username: usernameInput.value.trim(),
        file_size: done.reduce((sum, i) => sum + (selection.sizes.get(i.source) ?? 0), 0),
        status,
        retry_count: stats.retries,
        finished_at: 0,
        message: failed.length > 0 ? failed.map((i) => `${i.label}: ${i.message ?? ""}`).slice(0, 5).join("\n") : null,
        direction,
        batch: {
          label: batchLabel(items, selection.relDirs),
          file_count: items.length,
          done_count: done.length,
          failed_count: failed.length,
          roots: selection.roots,
        },
      },
    });
  } catch (error) {
    console.warn("failed to record batch history", error);
  }
  void loadHistory();
}


export async function finishQueue(
  items: QueueItem[],
  template: string,
  startedAt: number,
  direction: TransferDirection,
  selection: { relDirs: Map<string, string>; sizes: Map<string, number>; roots: string[]; baseDir: string },
) {
  const download = direction === "download";
  const done = items.filter((i) => i.status === "done");
  const stats: RunStats = {
    elapsedMs: Date.now() - startedAt,
    movedBytes: items.reduce((sum, i) => sum + (i.stats?.movedBytes ?? 0), 0),
    retries: items.reduce((sum, i) => sum + (i.stats?.retries ?? 0), 0),
  };
  const cancelled = items.some((i) => i.status === "cancelled");
  const total = items.length;
  // 批量中每个文件结束时不再各自刷新断点（见 handleProgressUpdate），整批结束后在这里统一读一次：
  // 被取消或失败的那个文件留下的断点要出现在状态面板和历史页里。
  void refreshCheckpointFromDisk().catch((error) => console.warn("failed to refresh checkpoint after batch", error));
  await recordBatchHistory(items, direction, selection, stats, cancelled);
  notifyBatchResult(items, direction, cancelled);
  renderQueueResultFiles(items);
  // 选择只保留没成功的文件：之后点「重试 / 继续传输」不会把已完成的文件再传一遍。
  const remaining = items.filter((i) => i.status !== "done").map((i) => i.source);
  const keep = remaining.length > 0 ? remaining : items.map((i) => i.source);
  const keepEntries = keep.map((path) => ({
    path,
    rel_dir: selection.relDirs.get(path) ?? "",
    size: selection.sizes.get(path) ?? 0,
  }));
  if (download) {
    setSelectedDownloads(keep, keepEntries);
  } else {
    setSelectedUploads(keep, keepEntries);
  }
  // 「当初选择的路径」保持原样，即使选择里只剩没成功的文件：重试产生的历史记录要靠它复用，
  // 换成剩余文件的列表会丢掉相对目录，之后复用时 dist/js/a.js 就被摊平到目标根目录了。
  if (download) state.downloadRoots = selection.roots;
  else state.uploadRoots = selection.roots;

  if (done.length === total) {
    // 全部送达：没有剩余工作，下次启动不该再提示继续。没全部成功时留着——
    // 用户在结果页上直接退出应用的话，下次启动还能接着传。
    clearSavedBatch();
    state.runPostCommandTemplate = template;
    await finishSuccessfulRun(
      download
        ? t("{total} 个文件全部下载完成", { total })
        : t("{total} 个文件全部上传完成", { total }),
      download
        ? t("已保存到 {dir}", { dir: localPathInput.value.trim() })
        : t("已传到 {p1}:{p2}", { p1: hostInput.value.trim(), p2: remotePathInput.value.trim() }),
      stats,
      done.map((i) => i.remote),
      done.map((i) => ({ local: i.local, remote: i.remote, skipped: i.identical })),
      verificationModeText(),
    );
    return;
  }
  // 部分失败时不执行命令：拿着不完整的文件去重启服务往往比不重启更糟。
  showCommandResult(null);
  // 校验徽章属于最后一个成功的文件，放在整体未成功的结果里会误导。
  hideVerifySummary();
  const failedCount = items.filter((i) => i.status === "failed").length;
  const otherCount = total - done.length - failedCount;
  const summary = [
    t("成功 {length} 个", { length: done.length }),
    failedCount > 0 ? t("失败 {failedCount} 个", { failedCount }) : "",
    otherCount > 0 ? t("未完成 {otherCount} 个", { otherCount }) : "",
  ]
    .filter(Boolean)
    .join("，");
  const commandNote = template ? t("\n因有文件未成功，传完后命令没有执行") : "";
  if (cancelled) {
    showResult("paused", download ? t("批量下载已取消") : t("批量上传已取消"), `${summary}${commandNote}`, stats);
  } else if (done.length === 0) {
    showResult("error", download ? t("批量下载失败") : t("批量上传失败"), `${summary}${commandNote}`, stats);
  } else {
    showResult(
      "warn",
      download ? t("部分文件下载失败") : t("部分文件上传失败"),
      `${summary}${commandNote}`,
      stats,
      false,
    );
  }
}


export const QUEUE_STATUS_LABEL: Record<QueueItemStatus, string> = {
  pending: t("未开始"),
  running: t("传输中"),
  done: t("完成"),
  failed: t("失败"),
  cancelled: t("已取消"),
};


export function renderQueueResultFiles(items: QueueItem[] | null) {
  if (!items) {
    resultFiles.classList.add("hidden");
    resultFiles.replaceChildren();
    return;
  }
  resultFiles.classList.remove("hidden");
  resultFiles.replaceChildren(
    ...items.map((item) => {
      const li = document.createElement("li");
      li.className = item.status;
      const mark = document.createElement("span");
      mark.className = "file-mark";
      const name = document.createElement("span");
      name.className = "file-name";
      name.textContent = item.label;
      name.title = item.source;
      const status = document.createElement("span");
      status.className = "file-status";
      status.textContent = item.message ?? QUEUE_STATUS_LABEL[item.status];
      status.title = status.textContent;
      li.append(mark, name, status);
      return li;
    }),
  );
}

