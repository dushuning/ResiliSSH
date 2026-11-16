/**
 * 分块地图：把文件按传输块（1 MB）画成网格，直观展示开始前已完成的部分、本次进度、
 * 正在传的块（并行传输时可以有好几处）、断线重连发生的位置，以及校验/完成状态。
 *
 * 设计取舍：
 * - 用 canvas 而不是成千上万个 DOM 节点：GB 级文件有几千块，DOM 方案每次进度刷新都会重排。
 * - 块太多放不下时，多块合成一格并按比例部分填充，保证任意大小文件都能一屏画完。
 * - 只依赖传入的数据，不感知页面结构，便于单独测试和替换。
 */

/**
 * 已完成的字节区间 [start, end)。一组区间按起点排序、互不重叠、相邻的已合并（与后端 RangeSet 一致）。
 * 多连接并行传输时各连接传文件的不同部分，进度不再是「开头连续的一段」，只能用区间表示。
 */
export type ByteRange = readonly [number, number];

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
/**
 * 同时标成「正在传输」的位置上限，与后端的并行连接数上限一致：再多就一定有已经没人在传的陈旧位置，
 * 只保留最近有进展的这么多个。
 */
const MAX_ACTIVE_REGIONS = 8;
/** 明暗主题写在根元素的这个属性上；它变了，缓存的配色就要重读。 */
const THEME_ATTRIBUTE = "data-theme";

export class BlockMap {
  private readonly ctx: CanvasRenderingContext2D;
  private totalBytes = 0;
  /** 已传字节总数（已完成的区间 + 各连接正在传的那一块里已传的部分）。是数量，不是文件偏移。 */
  private transferred = 0;
  /** 当前已完成的区间。后端没给区间的事件按「开头连续的一段」换算，两种来源走同一套绘制。 */
  private doneRanges: ByteRange[] = [];
  /**
   * 本次运行开始前就已完成的区间（首个进度事件带来的那些）：来自断点续传，或目标端旧版本里
   * 没变的块，用不同颜色区分。null 表示还没收到首个进度。
   */
  private resumedRanges: ByteRange[] | null = null;
  /** 本次运行是否收到过后端给的区间：收到过之后，不带区间的事件（校验、等待等）沿用上一次的区间。 */
  private hasRangeData = false;
  /** 首个进度事件只用来确定起点，不能把其中已有的区间当成「刚刚有进展」。 */
  private seeded = false;
  /** 最近有进展的区间末端，即各连接正在往后传的位置；最新的排在最后。 */
  private frontiers: number[] = [];
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
  /** 配色缓存：getComputedStyle 逐帧读 9 个变量并不便宜，而配色只在切换主题时才变。 */
  private colors: MapColors | null = null;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly blockBytes: number,
  ) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas 2d context unavailable");
    this.ctx = ctx;
    new ResizeObserver(() => {
      this.layout = null;
      this.draw(performance.now());
    }).observe(canvas);
    // 自己观察主题属性，而不是让设置模块来通知：手动切换和「跟随系统」最终都落在这个属性上，
    // 地图也就不必知道主题是谁、因为什么改的。
    new MutationObserver(() => {
      this.colors = null;
      this.requestFrame();
    }).observe(document.documentElement, { attributes: true, attributeFilter: [THEME_ATTRIBUTE] });
  }

  /**
   * 开始一次新的传输：清空上一轮的续传起点与重连标记。
   * knownStart 用于「重新接上一个已在进行的传输」（如页面刷新）：此时无从得知真实起点，
   * 传 0 让已传部分都算本次，避免把刷新时刻误画成续传起点。
   */
  reset(totalBytes = 0, knownStart: number | null = null) {
    this.totalBytes = totalBytes;
    this.transferred = 0;
    this.doneRanges = [];
    this.resumedRanges = knownStart === null ? null : prefixRanges(knownStart);
    this.hasRangeData = false;
    this.seeded = false;
    this.frontiers = [];
    this.retryOffsets = [];
    this.layout = null;
    this.setPhase("connecting");
  }

  /**
   * ranges 是后端给出的已完成区间；没有（旧式事件，或校验、等待这类与进度无关的事件）时
   * 退回「开头连续的 transferred 字节」，单连接下两者画出来完全一样。
   */
  update(transferred: number, total: number, ranges?: readonly ByteRange[]) {
    if (total > 0 && total !== this.totalBytes) {
      this.totalBytes = total;
      this.layout = null;
    }
    const next = this.resolveRanges(transferred, ranges);
    // 首个进度事件里已有的区间就是本次运行的起点；之后重连产生的进度不能覆盖它。
    if (this.resumedRanges === null) this.resumedRanges = next;
    if (this.seeded) this.trackFrontiers(next);
    this.seeded = true;
    this.doneRanges = next;
    this.transferred = transferred;
    this.requestFrame();
  }

  /** 把一次进度事件换算成已完成区间，见 update 的说明。 */
  private resolveRanges(transferred: number, ranges?: readonly ByteRange[]): ByteRange[] {
    // 空数组也是有效信息（「还没有任何块完成」）：引擎的进度事件总会带上区间，
    // 只有与进度无关的事件才不带。按「有没有这个字段」而不是「是否为空」区分，
    // 否则并行传输刚开始时，在途的字节会被画成文件开头的一小段。
    if (ranges) {
      this.hasRangeData = true;
      return [...ranges];
    }
    // 全部传完（进入整体校验或已完成）：这类事件不带区间，而上一次的区间可能还差最后几块。
    if (this.totalBytes > 0 && transferred >= this.totalBytes) return prefixRanges(this.totalBytes);
    // 不带区间、进度也没倒退：只是状态变化，沿用上一次的区间。
    if (this.hasRangeData && transferred >= totalOf(this.doneRanges)) return this.doneRanges;
    // 进度倒退（如目标端的半截数据丢了）或从未收到过区间：按开头连续的一段处理。
    this.hasRangeData = false;
    return prefixRanges(transferred);
  }

  /**
   * 找出「刚刚变长」的区间，它们的末端就是各连接正在往后传的位置。
   * 之所以要比较前后两次而不是把每个区间的末端都算上：续传或与旧版本比对后，地图上可能有
   * 成百上千个已完成的区间，其中绝大多数的后面并没有连接在传。
   */
  private trackFrontiers(next: ByteRange[]) {
    const grown = next
      .filter(([start, end]) => coveredBytes(this.doneRanges, start, end) < end - start)
      .map(([, end]) => end);
    const ends = new Set(next.map(([, end]) => end));
    // 被并进更长区间的旧位置已经不是末端了，去掉；仍是末端但这次没进展的保留（可能只是那条连接慢）。
    const kept = this.frontiers.filter((offset) => ends.has(offset) && !grown.includes(offset));
    this.frontiers = [...kept, ...grown]
      .filter((offset) => offset < this.totalBytes)
      .slice(-MAX_ACTIVE_REGIONS);
  }

  /** 第一个还没传的字节：单连接时就是当前进度位置，也是并行传输时队列里最靠前的那一段。 */
  private firstGapStart(): number {
    const first = this.doneRanges[0];
    return first && first[0] === 0 ? first[1] : 0;
  }

  /** 正在传输的位置：有进展记录时用它，否则（首个事件、单纯的等待）退回第一个没传的位置。 */
  private activeOffsets(): number[] {
    if (this.transferred >= this.totalBytes) return [];
    if (this.frontiers.length > 0) return this.frontiers;
    const gap = this.firstGapStart();
    return gap < this.totalBytes ? [gap] : [];
  }

  /**
   * 记一次断线重连；同一位置反复重连只记一次，避免标记堆叠。
   * 并行传输时后端不区分是哪条连接断的，统一记在第一个还没传的位置——单连接时它就是当前进度位置。
   */
  markRetry() {
    const offset = this.firstGapStart();
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

  /** 本次开始前是否已有完成的部分：决定图例里「此前已完成」是否有意义。 */
  get hasResumedPart(): boolean {
    return (this.resumedRanges?.length ?? 0) > 0;
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
    if (this.colors) return this.colors;
    const style = getComputedStyle(this.canvas);
    const read = (name: string) => style.getPropertyValue(name).trim();
    return this.colors = {
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
    const resumed = this.resumedRanges ?? [];
    const activeOffsets = this.activeOffsets();
    const activeCells = new Set(activeOffsets.map((o) => Math.floor(o / cellBytes)));
    // 各连接正在传的那一块里已传的字节：后端只给了总数，平均摊到各个正在传的位置上，
    // 让格子随传输平滑填充。单连接时只有一个位置，结果与按偏移计算完全相同。
    const inFlight = Math.max(0, this.transferred - totalOf(this.doneRanges));
    const inFlightEach = activeOffsets.length > 0 ? Math.min(this.blockBytes, inFlight / activeOffsets.length) : 0;
    const inFlightByCell = new Map<number, number>();
    for (const offset of activeOffsets) {
      const cell = Math.floor(offset / cellBytes);
      inFlightByCell.set(cell, (inFlightByCell.get(cell) ?? 0) + inFlightEach);
    }
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

      // 一格可能合并了多块、其中只有一部分完成：按完成的比例从左填充，不去画格子内部的分布。
      const doneBytes = coveredBytes(this.doneRanges, start, end);
      // 进度倒退后，起点里已经不算数的部分不能再画成「此前已完成」。
      const resumedFill = clamp01(Math.min(doneBytes, coveredBytes(resumed, start, end)) / span);
      const doneFill = clamp01((doneBytes + (inFlightByCell.get(i) ?? 0)) / span);
      if (doneFill > 0) this.fillCell(x, y, size * doneFill, size, doneColor);
      if (resumedFill > 0) this.fillCell(x, y, size * resumedFill, size, resumedColor);

      if (activeCells.has(i)) this.drawActiveCell(x, y, size, now, colors);

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

/** 文件开头连续的 length 字节，写成区间的形式。 */
function prefixRanges(length: number): ByteRange[] {
  return length > 0 ? [[0, length]] : [];
}

function totalOf(ranges: readonly ByteRange[]): number {
  return ranges.reduce((sum, [start, end]) => sum + (end - start), 0);
}

/**
 * [start, end) 中有多少字节落在 ranges 里。地图每帧要对每个格子问一次，区间又可能有上千个，
 * 所以先二分找到第一个可能相交的区间，再往后扫到越过 end 为止。
 */
function coveredBytes(ranges: readonly ByteRange[], start: number, end: number): number {
  let low = 0;
  let high = ranges.length;
  while (low < high) {
    const mid = (low + high) >> 1;
    if (ranges[mid][1] <= start) low = mid + 1;
    else high = mid;
  }
  let covered = 0;
  for (let i = low; i < ranges.length && ranges[i][0] < end; i++) {
    covered += Math.min(end, ranges[i][1]) - Math.max(start, ranges[i][0]);
  }
  return covered;
}
