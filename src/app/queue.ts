// 批量传输：把多个文件逐个交给单文件流程执行，并汇总结果。
import { t } from "../i18n";
import { clarifyTransferError } from "./backendText";
import { showCommandResult } from "./command";
import { prepareConnection } from "./connection";
import { hostInput, localPathInput, postCommandInput, queueChip, remotePathInput, resultFiles } from "./dom";
import { asDir, describeError, fileNameOf } from "./format";
import { setSelectedDownloads, setSelectedUploads } from "./paths";
import { hideVerifySummary } from "./progress";
import { state } from "./state";
import { finishSuccessfulRun, setTransferring, showResult, startOneDownload, startOneUpload } from "./transfer";
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
  const relDirs = new Map(state.uploadRelDirs);
  const sizes = new Map(state.uploadSizes);
  const queue = {
    items: sources.map<QueueItem>((source) => {
      const rel = download ? "" : (relDirs.get(source) ?? "");
      return {
        source,
        target: rel ? `${baseDir}${rel}/` : baseDir,
        label: rel ? `${rel}/${fileNameOf(source)}` : fileNameOf(source),
        remote: "",
        status: "pending",
      };
    }),
    index: 0,
  };
  state.activeQueue = queue;

  try {
    for (let i = 0; i < queue.items.length; i++) {
      const item = queue.items[i];
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
        // 与远端完全一致视为已送达：计入成功，队列结束后的命令照常执行。
        state.resolveQueueStep = null;
        item.status = "done";
        item.remote = state.runRemotePath;
        item.message = t("已是最新，跳过");
        continue;
      }
      item.remote = state.runRemotePath;
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
  await finishQueue(queue.items, template, queueStartedAt, direction, { relDirs, sizes });
}


export async function finishQueue(
  items: QueueItem[],
  template: string,
  startedAt: number,
  direction: TransferDirection,
  selection: { relDirs: Map<string, string>; sizes: Map<string, number> },
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
  renderQueueResultFiles(items);
  // 选择只保留没成功的文件：之后点「重试 / 继续传输」不会把已完成的文件再传一遍。
  const remaining = items.filter((i) => i.status !== "done").map((i) => i.source);
  const keep = remaining.length > 0 ? remaining : items.map((i) => i.source);
  if (download) {
    setSelectedDownloads(keep);
  } else {
    setSelectedUploads(
      keep,
      keep.map((path) => ({
        path,
        rel_dir: selection.relDirs.get(path) ?? "",
        size: selection.sizes.get(path) ?? 0,
      })),
    );
  }

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

