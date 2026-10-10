// 批量传输：把多个文件逐个交给单文件流程执行，并汇总结果。
import { t } from "../i18n";
import { invoke } from "@tauri-apps/api/core";
import { clarifyTransferError } from "./backendText";
import { showCommandResult } from "./command";
import { connectionPayload, prepareConnection } from "./connection";
import { showConfirm } from "./dialogs";
import { forceOverwriteInput, hostInput, localPathInput, portInput, postCommandInput, queueChip, remotePathInput, resultFiles, usernameInput } from "./dom";
import { asDir, describeError, fileNameOf } from "./format";
import { setSelectedDownloads, setSelectedUploads } from "./paths";
import { loadHistory } from "./history";
import { hideVerifySummary, setStatus } from "./progress";
import { state } from "./state";
import { finishSuccessfulRun, setTransferring, showResult, startOneDownload, startOneUpload, verificationModeText } from "./transfer";
import { QueueItem, QueueItemStatus, RunStats, RunStepResult, TransferDirection, UploadStart } from "./types";

/**
 * 多文件传输：逐个走单文件流程。某个文件失败不影响后面的文件；用户取消则整个队列停止。
 * 传完后命令只在队列结束后执行一次——部署场景下应当全部传完再重启服务。
 */
export async function startQueueFlow() {
  await prepareConnection();
  const direction = state.transferDirection;
  const download = direction === "download";
  // 多个文件必须放进目录，否则会互相覆盖成同一个文件。
  const baseInput = download ? localPathInput : remotePathInput;
  if (!baseInput.value.trim()) {
    throw new Error(download ? t("请填写本地保存路径") : t("请填写远端目录"));
  }
  const baseDir = asDir(baseInput.value);
  baseInput.value = baseDir;

  const queueStartedAt = Date.now();
  const template = postCommandInput.value.trim();
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
  // 设置后，逐个文件的预检不再弹确认框（见 confirmUploadAction / confirmDownloadAction）。
  state.activeQueue = queue;

  try {
    for (let i = 0; i < queue.items.length; i++) {
      const item = queue.items[i];
      // 上一个文件已结束（成功、跳过或失败都算），计入总进度；放在循环开头，continue 的分支也不会漏算。
      if (i > 0) queue.doneBytes += sizes.get(queue.items[i - 1].source) ?? 0;
      queue.index = i;
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
        state.resolveQueueStep = null;
        item.status = "skipped";
        item.message = t("已跳过");
        continue;
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
        for (const rest of queue.items.slice(i + 1)) {
          rest.status = "cancelled";
          rest.message = t("未开始");
        }
        break;
      }
    }
  } finally {
    state.resolveQueueStep = null;
    state.activeQueue = null;
    queueChip.classList.add("hidden");
    baseInput.value = baseDir;
  }
  await finishQueue(queue.items, template, queueStartedAt, direction, { relDirs, sizes, roots, baseDir });
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
        port: Number(portInput.value) || 22,
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
  await recordBatchHistory(items, direction, selection, stats, cancelled);
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
  // 有文件没成功时，选择只剩这些文件；下次的批量历史也应如实反映「只传了这些」。
  const nextRoots = remaining.length > 0 ? remaining : selection.roots;
  if (download) state.downloadRoots = nextRoots;
  else state.uploadRoots = nextRoots;

  if (done.length === total) {
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
  skipped: t("已跳过"),
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

