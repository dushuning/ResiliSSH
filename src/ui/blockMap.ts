/**
 * 分块地图：把文件按传输块（1 MB）画成网格，直观展示续传起点、本次进度、
 * 正在传的块、断线重连发生的位置，以及校验/完成状态。
 *
 * 设计取舍：
 * - 用 canvas 而不是成千上万个 DOM 节点：GB 级文件有几千块，DOM 方案每次进度刷新都会重排。
 * - 块太多放不下时，多块合成一格并按比例部分填充，保证任意大小文件都能一屏画完。
 * - 只依赖传入的数据，不感知页面结构，便于单独测试和替换。
 */

export type BlockMapPhase =
  | "connecting"
  | "transferring"
  | "waiting"
  | "verifying"
  | "completed"
  | "failed"
  | "cancelled";

interface MapColors {
  pending: string;
  resumed: string;
  resumedVerified: string;
  done: string;
  highlight: string;
  active: string;
  verified: string;
  retry: string;
  failed: string;
}

interface GridLayout {
  cell: number;
  gap: number;
  cols: number;
  cellCount: number;
  blocksPerCell: number;
  originX: number;
  originY: number;
}

/** 格子边长范围（CSS 像素）：太小看不清状态，太大小文件会显得空旷。 */
const MIN_CELL_PX = 6;
const MAX_CELL_PX = 26;
/** 格子小到一定程度时缩小间距，否则间距会吃掉大部分面积。 */
const SMALL_CELL_THRESHOLD_PX = 9;
const GAP_PX = 3;
const SMALL_GAP_PX = 2;
const CELL_RADIUS_RATIO = 0.22;
/** 动画帧间隔：进度可视化不需要 60fps，降低常驻传输时的 CPU 占用。 */
const FRAME_INTERVAL_MS = 40;
const PULSE_PERIOD_MS = 1200;
const WAITING_PULSE_PERIOD_MS = 2400;
const VERIFY_SWEEP_PERIOD_MS = 1800;
/** 完成时绿色波纹从首格扫到末格的总时长。 */
const COMPLETE_WAVE_MS = 900;
/** 完成高光带的宽度（占全部格子的比例），以及校验扫光带的宽度。 */
const COMPLETE_GLOW_BAND = 0.08;
const RETRY_DOT_RATIO = 0.36;
/** 当前块描边粗细（相对格子边长）与呼吸时的最低不透明度。 */
const ACTIVE_RING_RATIO = 0.16;
const ACTIVE_RING_MIN_ALPHA = 0.45;
/** 格子小于这个尺寸时角标圆点会小到看不见，改为整格涂成重连色。 */
const RETRY_DOT_MIN_CELL_PX = 14;

export class BlockMap {
  private readonly ctx: CanvasRenderingContext2D;
  private totalBytes = 0;
  private transferred = 0;
  /** 本次运行的起点：之前的进度来自断点续传，用不同颜色区分。null 表示还没收到首个进度。 */
  private resumedFrom: number | null = null;
  private retryOffsets: number[] = [];
  /**
   * 弱网可靠模式下每块写完即读回比对，「已传」就等于「已校验」，直接画成绿色；
   * 快速模式只在最后整体校验，传完的块先画蓝色，全部校验通过后才变绿。
   */
  private verifiedPerBlock = true;
  private phase: BlockMapPhase = "connecting";
  private phaseSince = performance.now();
  private layout: GridLayout | null = null;
  private frameHandle: number | null = null;
  private lastFrameAt = 0;
  private readonly reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  private readonly resizeObserver: ResizeObserver;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly blockBytes: number,
  ) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas 2d context unavailable");
    this.ctx = ctx;
    this.resizeObserver = new ResizeObserver(() => {
      this.layout = null;
      this.draw(performance.now());
    });
    this.resizeObserver.observe(canvas);
  }

  /**
   * 开始一次新的传输：清空上一轮的续传起点与重连标记。
   * knownStart 用于「重新接上一个已在进行的传输」（如页面刷新）：此时无从得知真实起点，
   * 传 0 让已传部分都算本次，避免把刷新时刻误画成续传起点。
   */
  reset(totalBytes = 0, knownStart: number | null = null) {
    this.totalBytes = totalBytes;
    this.transferred = 0;
    this.resumedFrom = knownStart;
    this.retryOffsets = [];
    this.layout = null;
    this.setPhase("connecting");
  }

  update(transferred: number, total: number) {
    if (total > 0 && total !== this.totalBytes) {
      this.totalBytes = total;
      this.layout = null;
    }
    // 首个进度事件就是后端解析出的续传偏移；之后重连产生的进度不能覆盖它。
    if (this.resumedFrom === null) this.resumedFrom = transferred;
    this.transferred = transferred;
    this.requestFrame();
  }

  /** 在当前进度位置记一次断线重连；同一位置反复重连只记一次，避免标记堆叠。 */
  markRetry() {
    const offset = this.transferred;
    if (this.retryOffsets[this.retryOffsets.length - 1] !== offset) {
      this.retryOffsets.push(offset);
    }
    this.requestFrame();
  }

  setPhase(phase: BlockMapPhase) {
    if (phase === this.phase) return;
    this.phase = phase;
    this.phaseSince = performance.now();
    this.requestFrame();
  }

  get blockCount(): number {
    return this.totalBytes > 0 ? Math.ceil(this.totalBytes / this.blockBytes) : 0;
  }

  get blocksPerCell(): number {
    return this.ensureLayout()?.blocksPerCell ?? 1;
  }

  get retryCount(): number {
    return this.retryOffsets.length;
  }

  setVerifiedPerBlock(value: boolean) {
    this.verifiedPerBlock = value;
    this.requestFrame();
  }

  get verifiesPerBlock(): boolean {
    return this.verifiedPerBlock;
  }

  /** 本次是否从断点续传（起点 > 0）：决定图例里「之前已续传」是否有意义。 */
  get hasResumedPart(): boolean {
    return (this.resumedFrom ?? 0) > 0;
  }

  dispose() {
    this.resizeObserver.disconnect();
    if (this.frameHandle !== null) cancelAnimationFrame(this.frameHandle);
  }

  private needsAnimation(now: number): boolean {
    if (this.reducedMotion.matches) return false;
    if (this.phase === "completed") return now - this.phaseSince < COMPLETE_WAVE_MS;
    return this.phase === "transferring" || this.phase === "waiting" || this.phase === "verifying";
  }

  private requestFrame() {
    if (this.frameHandle !== null) return;
    this.frameHandle = requestAnimationFrame((now) => this.onFrame(now));
  }

  private onFrame(now: number) {
    this.frameHandle = null;
    if (now - this.lastFrameAt >= FRAME_INTERVAL_MS || !this.needsAnimation(now)) {
      this.lastFrameAt = now;
      this.draw(now);
    }
    if (this.needsAnimation(now)) this.requestFrame();
  }

  private ensureLayout(): GridLayout | null {
    if (this.layout) return this.layout;
    const width = this.canvas.clientWidth;
    const height = this.canvas.clientHeight;
    const blocks = this.blockCount;
    if (width <= 0 || height <= 0 || blocks === 0) return null;

    // 从大到小找第一个能把所有块放进画布的格子尺寸；连最小格子都放不下时才合并多块为一格。
    for (let cell = MAX_CELL_PX; cell >= MIN_CELL_PX; cell--) {
      const gap = cell < SMALL_CELL_THRESHOLD_PX ? SMALL_GAP_PX : GAP_PX;
      const cols = Math.floor((width + gap) / (cell + gap));
      const rows = Math.floor((height + gap) / (cell + gap));
      if (cols * rows >= blocks) {
        return (this.layout = this.centered(cell, gap, cols, blocks, 1, width, height));
      }
    }
    const gap = SMALL_GAP_PX;
    const cols = Math.floor((width + gap) / (MIN_CELL_PX + gap));
    const rows = Math.floor((height + gap) / (MIN_CELL_PX + gap));
    const blocksPerCell = Math.ceil(blocks / Math.max(1, cols * rows));
    const cellCount = Math.ceil(blocks / blocksPerCell);
    return (this.layout = this.centered(MIN_CELL_PX, gap, cols, cellCount, blocksPerCell, width, height));
  }

  private centered(
    cell: number,
    gap: number,
    maxCols: number,
    cellCount: number,
    blocksPerCell: number,
    width: number,
    height: number,
  ): GridLayout {
    const cols = Math.min(maxCols, cellCount);
    const rows = Math.ceil(cellCount / cols);
    const gridW = cols * cell + (cols - 1) * gap;
    const gridH = rows * cell + (rows - 1) * gap;
    return {
      cell,
      gap,
      cols,
      cellCount,
      blocksPerCell,
      originX: Math.max(0, Math.floor((width - gridW) / 2)),
      originY: Math.max(0, Math.floor((height - gridH) / 2)),
    };
  }

  private readColors(): MapColors {
    const style = getComputedStyle(this.canvas);
    const read = (name: string) => style.getPropertyValue(name).trim();
    return {
      pending: read("--map-pending"),
      resumed: read("--map-resumed"),
      resumedVerified: read("--map-resumed-verified"),
      done: read("--map-done"),
      highlight: read("--map-highlight"),
      active: read("--map-active"),
      verified: read("--map-verified"),
      retry: read("--map-retry"),
      failed: read("--map-failed"),
    };
  }

  private draw(now: number) {
    const dpr = window.devicePixelRatio || 1;
    const width = this.canvas.clientWidth;
    const height = this.canvas.clientHeight;
    if (this.canvas.width !== Math.round(width * dpr) || this.canvas.height !== Math.round(height * dpr)) {
      this.canvas.width = Math.round(width * dpr);
      this.canvas.height = Math.round(height * dpr);
    }
    const ctx = this.ctx;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);

    const layout = this.ensureLayout();
    if (!layout) return;
    const colors = this.readColors();
    const cellBytes = layout.blocksPerCell * this.blockBytes;
    const resumed = this.resumedFrom ?? 0;
    const activeIndex =
      this.transferred < this.totalBytes ? Math.floor(this.transferred / cellBytes) : -1;
    const retryCells = new Set(this.retryOffsets.map((o) => Math.min(Math.floor(o / cellBytes), layout.cellCount - 1)));
    const animate = !this.reducedMotion.matches;
    const sinceMs = now - this.phaseSince;
    const doneColor = this.verifiedPerBlock ? colors.verified : colors.done;
    const resumedColor = this.verifiedPerBlock ? colors.resumedVerified : colors.resumed;

    for (let i = 0; i < layout.cellCount; i++) {
      const start = i * cellBytes;
      const end = Math.min(this.totalBytes, start + cellBytes);
      const span = Math.max(1, end - start);
      const x = layout.originX + (i % layout.cols) * (layout.cell + layout.gap);
      const y = layout.originY + Math.floor(i / layout.cols) * (layout.cell + layout.gap);
      const size = layout.cell;

      this.fillCell(x, y, size, size, colors.pending);

      if (this.phase === "completed") {
        // 完成波纹：一道高光从首格扫到末格，扫过的格子定格为绿色。
        // 可靠模式下格子本来就是绿的，靠高光带让「全部通过」这一刻仍然看得见。
        const position = i / layout.cellCount;
        const front = animate ? sinceMs / COMPLETE_WAVE_MS : Infinity;
        this.fillCell(x, y, size, size, position <= front ? colors.verified : doneColor);
        const distance = front - position;
        if (animate && distance >= 0 && distance < COMPLETE_GLOW_BAND) {
          this.ctx.globalAlpha = 1 - distance / COMPLETE_GLOW_BAND;
          this.fillCell(x, y, size, size, colors.highlight);
          this.ctx.globalAlpha = 1;
        }
        continue;
      }

      const resumedFill = clamp01((Math.min(end, resumed) - start) / span);
      const doneFill = clamp01((Math.min(end, this.transferred) - start) / span);
      if (doneFill > 0) this.fillCell(x, y, size * doneFill, size, doneColor);
      if (resumedFill > 0) this.fillCell(x, y, size * resumedFill, size, resumedColor);

      if (i === activeIndex) this.drawActiveCell(x, y, size, now, colors);

      if (this.phase === "verifying" && animate && doneFill > 0) {
        // 全文件校验没有细粒度进度，用一道扫光表示「正在逐块重读比对」。
        const sweep = (now % VERIFY_SWEEP_PERIOD_MS) / VERIFY_SWEEP_PERIOD_MS;
        const distance = Math.abs(i / layout.cellCount - sweep);
        if (distance < 0.06) {
          this.ctx.globalAlpha = 0.55 * (1 - distance / 0.06);
          this.fillCell(x, y, size, size, colors.verified);
          this.ctx.globalAlpha = 1;
        }
      }

      if (retryCells.has(i)) this.drawRetryDot(x, y, size, colors.retry);
    }
  }

  private drawActiveCell(x: number, y: number, size: number, now: number, colors: MapColors) {
    if (this.phase === "failed") {
      this.fillCell(x, y, size, size, colors.failed);
      return;
    }
    if (this.phase === "cancelled") return;
    // 当前块用「描边 + 呼吸」表示，而不是整块半透明填充：
    // 半透明的蓝会和「之前已续传」的浅蓝混淆，描边在任何进度下都一眼可辨。
    const waiting = this.phase === "waiting" || this.phase === "connecting";
    const color = waiting ? colors.retry : colors.active;
    const wave = this.reducedMotion.matches
      ? 1
      : (Math.sin((now / (waiting ? WAITING_PULSE_PERIOD_MS : PULSE_PERIOD_MS)) * Math.PI * 2) + 1) / 2;
    const lineWidth = Math.max(1.5, size * ACTIVE_RING_RATIO);
    const inset = lineWidth / 2;
    this.ctx.globalAlpha = ACTIVE_RING_MIN_ALPHA + (1 - ACTIVE_RING_MIN_ALPHA) * wave;
    this.ctx.strokeStyle = color;
    this.ctx.lineWidth = lineWidth;
    this.ctx.beginPath();
    const side = size - lineWidth;
    if (typeof this.ctx.roundRect === "function") {
      this.ctx.roundRect(x + inset, y + inset, side, side, side * CELL_RADIUS_RATIO);
    } else {
      this.ctx.rect(x + inset, y + inset, side, side);
    }
    this.ctx.stroke();
    this.ctx.globalAlpha = 1;
  }

  private drawRetryDot(x: number, y: number, size: number, color: string) {
    if (size < RETRY_DOT_MIN_CELL_PX) {
      this.fillCell(x, y, size, size, color);
      return;
    }
    const r = Math.max(1.5, (size * RETRY_DOT_RATIO) / 2);
    this.ctx.fillStyle = color;
    this.ctx.beginPath();
    this.ctx.arc(x + size - r, y + r, r, 0, Math.PI * 2);
    this.ctx.fill();
  }

  private fillCell(x: number, y: number, w: number, h: number, color: string) {
    if (w <= 0) return;
    const ctx = this.ctx;
    ctx.fillStyle = color;
    const radius = Math.min(w, h) * CELL_RADIUS_RATIO;
    if (radius >= 1 && typeof ctx.roundRect === "function") {
      ctx.beginPath();
      ctx.roundRect(x, y, w, h, radius);
      ctx.fill();
    } else {
      ctx.fillRect(x, y, w, h);
    }
  }
}

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}
