// 运行时可变状态的唯一来源：各模块通过 state.xxx 读写，避免模块间各自持有副本。
import { ActiveQueue, ConnectionProfile, DownloadCheckpoint, PageId, RemoteDirListing, RemoteDownloadProbe, RemoteUploadProbe, RunReceipt, RunStepResult, TransferDirection, TransferView, UploadCheckpoint, UploadHistoryEntry } from "./types";

/**
 * 运行时可变状态：集中在一个对象里，各模块通过 state.xxx 读写。
 * （ES 模块不能对导入的变量重新赋值，所以不能把 let 直接 export 给别的模块改。）
 */
export const state = {
  isTransferring: false,
  cancelInFlight: false,
  isReconnecting: false,
  /** 传输已发起但尚未收到首条进度：与 stall 等待区分，避免误判为卡死 */
  isConnecting: false,
  /** 块读回校验进行中：对用户透明，仅用于抑制误报 stall */
  isChunkVerifying: false,
  /** 全文件 SHA-256 校验进行中 */
  isFullVerifying: false,
  retryDelayMs: 0,
  retryScheduledAt: 0,
  transferDirection: "upload" as TransferDirection,
  retryCount: 0,
  historyEntries: [] as UploadHistoryEntry[],
  savedProfiles: [] as ConnectionProfile[],
  activeUploadCheckpoint: null as UploadCheckpoint | null,
  activeDownloadCheckpoint: null as DownloadCheckpoint | null,
  lastProbe: null as RemoteUploadProbe | RemoteDownloadProbe | null,
  lastProgressEventAt: 0,
  speedSamples: [] as { t: number; bytes: number }[],
  speedDisplayTimer: null as number | null,
  progressWatchdogTimer: null as number | null,
  currentPage: "transfer" as PageId,
  currentView: "compose" as TransferView,
  /** 当前这次传输的总字节与起止信息：结果页的耗时、平均速度、本次实传量都从这里算。 */
  progressTotalBytes: 0,
  progressTransferredBytes: 0,
  runStartedAt: 0,
  runStartBytes: null as number | null,
  /** 传输开始时锁定方向：取消、结果展示都以它为准，不受之后界面状态影响。 */
  runDirection: "upload" as TransferDirection,
  /** 本次传输的远端完整路径（预检解析后的结果），传完后命令的变量从这里取。 */
  runRemotePath: "",
  /** 本次传输实际的本地文件（下载时由预检解析出的保存路径）。 */
  runLocalPath: "",
  /** 最近一次完整成功的传输，结果页的凭证按钮据此生成内容。 */
  lastReceipt: null as RunReceipt | null,
  /** 传输开始时锁定的「传完后执行」命令：传输中输入框已锁定，这里只是避免读到之后的改动。 */
  runPostCommandTemplate: "",
  /** 传完后命令执行中：此时传输已结束（isTransferring=false），但界面仍停留在进行中视图。 */
  commandRunning: false,
  /**
   * 待上传的本地文件。单文件时与 localPathInput 一致；多选/多拖入时其余文件也在这里。
   * localPathInput 始终保存第一个文件，原有的单文件逻辑（预检、路径预览）无需感知队列。
   */
  selectedUploads: [] as string[],
  /** 文件夹上传时每个文件相对所选文件夹的目录（如 dist/js），用于在远端还原目录结构；直接选的文件不在表中。 */
  uploadRelDirs: new Map<string, string>(),
  uploadSizes: new Map<string, number>(),
  /** 批量下载时选中的远端文件；remotePathInput 始终保存第一个，单文件逻辑不受影响。 */
  selectedDownloads: [] as string[],
  /** 文件夹下载时每个远端文件相对所选文件夹的目录，用于在本地还原目录结构；与 uploadRelDirs 对称。 */
  downloadRelDirs: new Map<string, string>(),
  /** 当初选择的路径（拖入的文件夹、勾选的文件等），批量历史据此一键复用。 */
  uploadRoots: [] as string[],
  downloadRoots: [] as string[],
  downloadSizes: new Map<string, number>(),
  /** 队列执行中的状态；为 null 表示当前是普通单文件传输。 */
  activeQueue: null as ActiveQueue | null,
  resolveQueueStep: null as ((result: RunStepResult) => void) | null,
  probeDebounceTimer: null as number | null,
  backgroundProbeGeneration: 0,
  confirmResolver: null as ((value: boolean) => void) | null,
  confirmClosing: false,
  remoteBrowseCurrent: null as RemoteDirListing | null,
  remoteBrowseLoading: false,
  /** 开始流程里有预检、确认框等多个 await，期间按钮仍可点；不加闸门的话双击会并发出第二个流程。 */
  startInFlight: false,
};
