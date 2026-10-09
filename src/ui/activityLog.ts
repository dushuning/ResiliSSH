/**
 * 「日志」页：展示后端推送的活动日志（连接、认证、指纹、网络、权限、传输、校验）。
 * 只依赖传入的 DOM 节点，不感知其他页面；main.ts 负责把事件接进来。
 */

export type LogLevel = "info" | "success" | "warn" | "error";
export type LogCategory =
  | "connect"
  | "auth"
  | "host_key"
  | "network"
  | "permission"
  | "transfer"
  | "verify"
  | "command";
export type LogFilter = "all" | "warn" | "error";

export interface ActivityEntry {
  id: number;
  ts_ms: number;
  level: LogLevel;
  category: LogCategory;
  message: string;
}

const CATEGORY_LABEL: Record<LogCategory, string> = {
  connect: "连接",
  auth: "认证",
  host_key: "指纹",
  network: "网络",
  permission: "权限",
  transfer: "传输",
  verify: "校验",
  command: "命令",
};

const LEVEL_LABEL: Record<LogLevel, string> = {
  info: "信息",
  success: "成功",
  warn: "警告",
  error: "错误",
};

/** 筛选按严重程度阈值：「警告及以上」包含错误。 */
const LEVEL_RANK: Record<LogLevel, number> = { info: 0, success: 0, warn: 1, error: 2 };
const FILTER_MIN_RANK: Record<LogFilter, number> = { all: 0, warn: 1, error: 2 };

/** 与后端环形缓冲区容量一致，界面不比后端保留得更多。 */
const MAX_ENTRIES = 500;
/** 距底部小于这个距离视为「正在看最新」，新日志到来时自动跟随滚动。 */
const FOLLOW_THRESHOLD_PX = 24;

function formatClock(tsMs: number): string {
  return new Date(tsMs).toLocaleTimeString("zh-CN", { hour12: false });
}

export class ActivityLogView {
  private entries: ActivityEntry[] = [];
  private lastId = 0;
  private filter: LogFilter = "all";
  private visible = false;
  private unreadErrors = 0;
  private following = true;

  constructor(
    private readonly list: HTMLElement,
    private readonly jumpButton: HTMLButtonElement,
    private readonly unreadBadge: HTMLElement,
  ) {
    list.addEventListener("scroll", () => {
      this.following = this.isNearBottom();
      if (this.following) this.jumpButton.classList.add("hidden");
    });
    jumpButton.addEventListener("click", () => this.scrollToBottom());
    this.render();
  }

  /** 页面加载时补拉后端缓冲里的历史；与实时事件按 id 去重，先监听后补拉也不会重复。 */
  load(entries: ActivityEntry[]) {
    const merged = new Map<number, ActivityEntry>();
    for (const e of [...entries, ...this.entries]) merged.set(e.id, e);
    this.entries = [...merged.values()].sort((a, b) => a.id - b.id).slice(-MAX_ENTRIES);
    this.lastId = this.entries[this.entries.length - 1]?.id ?? this.lastId;
    this.render();
  }

  append(entry: ActivityEntry) {
    if (entry.id <= this.lastId) return;
    this.lastId = entry.id;
    this.entries.push(entry);
    if (this.entries.length > MAX_ENTRIES) this.entries.shift();

    if (entry.level === "error" && !this.visible) {
      this.unreadErrors += 1;
      this.updateBadge();
    }
    if (!this.matches(entry)) return;

    this.list.querySelector(".list-empty")?.remove();
    this.list.append(this.renderRow(entry));
    while (this.list.childElementCount > MAX_ENTRIES) this.list.firstElementChild?.remove();

    if (this.following) this.scrollToBottom();
    else this.jumpButton.classList.remove("hidden");
  }

  setFilter(filter: LogFilter) {
    this.filter = filter;
    this.render();
  }

  /** 进入日志页即视为已读；离开后再出现的错误才计入侧边栏红点。 */
  setVisible(visible: boolean) {
    this.visible = visible;
    if (!visible) return;
    this.unreadErrors = 0;
    this.updateBadge();
    this.scrollToBottom();
  }

  clear() {
    this.entries = [];
    this.unreadErrors = 0;
    this.updateBadge();
    this.render();
  }

  /** 复制当前筛选结果，格式便于直接贴到聊天或工单里求助。 */
  toText(): string {
    return this.entries
      .filter((e) => this.matches(e))
      .map(
        (e) =>
          `${formatClock(e.ts_ms)} [${LEVEL_LABEL[e.level]}][${CATEGORY_LABEL[e.category]}] ${e.message}`,
      )
      .join("\n");
  }

  private matches(entry: ActivityEntry): boolean {
    return LEVEL_RANK[entry.level] >= FILTER_MIN_RANK[this.filter];
  }

  private render() {
    const visible = this.entries.filter((e) => this.matches(e));
    if (visible.length === 0) {
      const empty = document.createElement("div");
      empty.className = "list-empty";
      empty.textContent =
        this.entries.length === 0
          ? "暂无日志\n连接、认证、传输、重连、校验的每一步都会记录在这里"
          : "当前筛选条件下没有日志";
      this.list.replaceChildren(empty);
      this.jumpButton.classList.add("hidden");
      return;
    }
    this.list.replaceChildren(...visible.map((e) => this.renderRow(e)));
    this.scrollToBottom();
  }

  private renderRow(entry: ActivityEntry): HTMLElement {
    const row = document.createElement("div");
    row.className = `log-row ${entry.level}`;
    const time = document.createElement("span");
    time.className = "log-time";
    time.textContent = formatClock(entry.ts_ms);
    const level = document.createElement("span");
    level.className = "log-level";
    level.title = LEVEL_LABEL[entry.level];
    const category = document.createElement("span");
    category.className = "log-category";
    category.textContent = CATEGORY_LABEL[entry.category] ?? entry.category;
    const message = document.createElement("span");
    message.className = "log-message";
    // 消息里可能含远端路径、服务器返回的错误文本，必须按纯文本渲染。
    message.textContent = entry.message;
    row.append(time, level, category, message);
    return row;
  }

  private isNearBottom(): boolean {
    const { scrollTop, scrollHeight, clientHeight } = this.list;
    return scrollHeight - scrollTop - clientHeight < FOLLOW_THRESHOLD_PX;
  }

  private scrollToBottom() {
    this.list.scrollTop = this.list.scrollHeight;
    this.following = true;
    this.jumpButton.classList.add("hidden");
  }

  private updateBadge() {
    this.unreadBadge.textContent = String(this.unreadErrors);
    this.unreadBadge.classList.toggle("hidden", this.unreadErrors === 0);
  }
}

/** WKWebView 中 Clipboard API 偶尔不可用（权限/焦点），退回 execCommand 保证复制可用。 */
export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
}
