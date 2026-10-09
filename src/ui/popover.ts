/**
 * 轻量下拉面板：常用目录、常用命令共用。原生 <select>/<datalist> 无法在条目上放「删除」按钮，
 * 也不能分组显示「收藏 / 最近」，所以自己渲染。
 */

export interface PopoverItem {
  label: string;
  title?: string;
  onSelect: () => void;
  /** 提供时在条目右侧显示删除按钮。 */
  onRemove?: () => void;
}

export interface PopoverSection {
  heading?: string;
  items: PopoverItem[];
}

export class Popover {
  private open = false;

  constructor(
    private readonly panel: HTMLElement,
    /** 点击这个容器之外的地方关闭面板（通常是触发按钮与面板的共同父节点）。 */
    private readonly boundary: HTMLElement,
  ) {
    document.addEventListener("mousedown", (event) => {
      if (this.open && !this.boundary.contains(event.target as Node)) this.close();
    });
    document.addEventListener("keydown", (event) => {
      if (this.open && event.key === "Escape") {
        event.stopPropagation();
        this.close();
      }
    });
  }

  get isOpen(): boolean {
    return this.open;
  }

  /** footer 用于放说明性文字（如可用变量），始终显示在列表下方。 */
  show(sections: PopoverSection[], emptyText: string, footer?: HTMLElement) {
    const nonEmpty = sections.filter((s) => s.items.length > 0);
    if (nonEmpty.length === 0) {
      const empty = document.createElement("div");
      empty.className = "popover-empty";
      empty.textContent = emptyText;
      this.panel.replaceChildren(empty);
    } else {
      this.panel.replaceChildren(...nonEmpty.map((section) => this.renderSection(section)));
    }
    if (footer) {
      footer.classList.add("popover-footer");
      this.panel.append(footer);
    }
    this.panel.classList.remove("hidden");
    this.open = true;
    this.panel.querySelector<HTMLElement>(".popover-item")?.focus();
  }

  close() {
    this.panel.classList.add("hidden");
    this.open = false;
  }

  private renderSection(section: PopoverSection): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "popover-section";
    if (section.heading) {
      const heading = document.createElement("div");
      heading.className = "popover-heading";
      heading.textContent = section.heading;
      wrap.append(heading);
    }
    for (const item of section.items) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "popover-item";
      row.setAttribute("role", "menuitem");
      row.title = item.title ?? item.label;
      const label = document.createElement("span");
      label.className = "popover-item-label";
      // 路径、命令都来自用户输入或服务器，按纯文本渲染。
      label.textContent = item.label;
      row.append(label);
      row.addEventListener("click", () => {
        this.close();
        item.onSelect();
      });
      if (item.onRemove) {
        const remove = document.createElement("span");
        remove.className = "popover-item-remove";
        remove.title = "移除";
        remove.textContent = "×";
        remove.addEventListener("click", (event) => {
          // 删除不应同时触发「选中」。
          event.stopPropagation();
          item.onRemove?.();
        });
        row.append(remove);
      }
      wrap.append(row);
    }
    return wrap;
  }
}
