// 未完成批量的持久化：把「还剩哪些文件要传」存进 localStorage，应用退出后再打开能接着传。
// 这里只管存取，不碰界面；什么时候存、怎么恢复由 queue.ts 决定。
import { BATCH_STORAGE_MAX_CHARS } from "./constants";
import { LocalEntry, TransferDirection } from "./types";

/** 批量的描述（文件列表等）：开始时写一次，之后不变。 */
export const BATCH_STORAGE_KEY = "resiliss-batch";

/** 每个文件的状态，一个字符对应描述里的一个文件：每传完一个文件都要重写，所以单独存，不必重写整个列表。 */
export const BATCH_STATUS_STORAGE_KEY = "resiliss-batch-status";

/** 存储格式的版本：结构变了就加一，读到别的版本时直接丢弃，不去猜旧数据的含义。 */
const BATCH_FORMAT_VERSION = 1;

/** 正在传和被取消的文件都算 pending：它们的进度在后端断点里，恢复时经正常预检续传。 */
export type SavedFileStatus = "pending" | "done" | "failed";

const STATUS_CODES: Record<SavedFileStatus, string> = { pending: "p", done: "d", failed: "f" };

const STATUS_BY_CODE = new Map(
  (Object.entries(STATUS_CODES) as [SavedFileStatus, string][]).map(([status, code]) => [code, status]),
);


/**
 * 重建剩余工作所需的全部信息。不含密码和口令：服务器只记 id，凭据在恢复时照常从钥匙串读；
 * host/port/username 用于核对「这台服务器还是不是当初那台」。
 */
export interface SavedBatch {
  version: number;
  direction: TransferDirection;
  server: { profileId: string; host: string; port: number; username: string };
  /** 目标目录（上传为远端目录，下载为本地目录）。 */
  baseDir: string;
  /** 当初选择的路径：恢复后产生的批量历史仍要靠它一键复用。 */
  roots: string[];
  postCommand: string;
  reliable: boolean;
  /** 本轮要传的文件，含相对目录（还原目录结构）与大小（总进度）。 */
  files: LocalEntry[];
  /** 同一个批量在之前几轮（重试、继续）里已经完成的文件数，只用于提示「共多少个」。 */
  completedBefore: number;
}


export interface LoadedBatch extends SavedBatch {
  statuses: SavedFileStatus[];
}


function encodeStatuses(statuses: SavedFileStatus[]): string {
  return statuses.map((status) => STATUS_CODES[status]).join("");
}


/**
 * 保存一个刚开始的批量，返回是否存上了。太大（或存储已满）时放弃并返回 false：
 * 不能续批量只是少一项便利，不值得为此打断传输或提示用户。
 * 无论成败都先清掉旧的：新批量开始了，上一个就不该再被提示继续。
 */
export function saveBatch(batch: Omit<SavedBatch, "version">, statuses: SavedFileStatus[]): boolean {
  clearSavedBatch();
  const serialized = JSON.stringify({ ...batch, version: BATCH_FORMAT_VERSION });
  if (serialized.length > BATCH_STORAGE_MAX_CHARS) {
    console.info("batch too large to persist", { files: batch.files.length, chars: serialized.length });
    return false;
  }
  try {
    localStorage.setItem(BATCH_STORAGE_KEY, serialized);
    localStorage.setItem(BATCH_STATUS_STORAGE_KEY, encodeStatuses(statuses));
    return true;
  } catch (error) {
    // 只写进去一半比没写更糟：读回来会对不上。
    clearSavedBatch();
    console.warn("failed to persist batch", { files: batch.files.length, error });
    return false;
  }
}


/** 更新各文件的状态。调用方保证顺序与 saveBatch 时的文件列表一致，且 saveBatch 当时返回了 true。 */
export function saveBatchStatuses(statuses: SavedFileStatus[]) {
  try {
    localStorage.setItem(BATCH_STATUS_STORAGE_KEY, encodeStatuses(statuses));
  } catch (error) {
    console.warn("failed to persist batch progress", { files: statuses.length, error });
  }
}


export function clearSavedBatch() {
  localStorage.removeItem(BATCH_STORAGE_KEY);
  localStorage.removeItem(BATCH_STATUS_STORAGE_KEY);
}


/**
 * 读回保存的批量。任何地方对不上（损坏、别的版本、状态与文件数不一致）都当作没有并清掉：
 * 拿着半懂不懂的数据去传文件，比不提示继续糟糕得多。
 */
export function loadSavedBatch(): LoadedBatch | null {
  const serialized = localStorage.getItem(BATCH_STORAGE_KEY);
  if (serialized === null) return null;
  try {
    const batch = JSON.parse(serialized) as SavedBatch;
    const codes = localStorage.getItem(BATCH_STATUS_STORAGE_KEY) ?? "";
    const statuses = [...codes].map((code) => STATUS_BY_CODE.get(code));
    const valid =
      batch.version === BATCH_FORMAT_VERSION &&
      (batch.direction === "upload" || batch.direction === "download") &&
      typeof batch.server?.profileId === "string" &&
      typeof batch.baseDir === "string" &&
      Array.isArray(batch.roots) &&
      Array.isArray(batch.files) &&
      batch.files.every((file) => typeof file?.path === "string" && typeof file.rel_dir === "string") &&
      statuses.length === batch.files.length &&
      statuses.every((status) => status !== undefined);
    if (valid) return { ...batch, statuses: statuses as SavedFileStatus[] };
    console.warn("discarding unreadable saved batch", { version: batch.version, files: batch.files?.length });
  } catch (error) {
    console.warn("discarding corrupt saved batch", error);
  }
  clearSavedBatch();
  return null;
}
