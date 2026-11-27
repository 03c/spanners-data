/**
 * Minimal virtual grid: only the visible rows exist in the DOM.
 *
 * This is the same technique the real app will use, and it is the thing that
 * makes a webview viable at 100k rows. If this is slow, the shell decision is
 * already made.
 */
export class VirtualGrid {
  private readonly rowH: number;
  private startRow = -1;

  constructor(
    private readonly scroller: HTMLElement,
    private readonly body: HTMLElement,
    private readonly spacer: HTMLElement,
    private readonly totalRows: number,
    private readonly cols: number,
    private readonly cell: (row: number, col: number) => string,
    rowH = 22,
  ) {
    this.rowH = rowH;
    this.spacer.style.height = `${totalRows * rowH}px`;
    this.scroller.addEventListener("scroll", () => this.render(), { passive: true });
    this.render();
  }

  scrollTo(offset: number): void {
    this.scroller.scrollTop = offset;
    this.render();
  }

  render(): void {
    const viewportH = this.scroller.clientHeight || 600;
    const top = this.scroller.scrollTop;
    const visible = Math.ceil(viewportH / this.rowH) + 1;
    let start = Math.floor(top / this.rowH) - 1;
    start = Math.max(0, Math.min(start, this.totalRows - visible));

    // Cheap early-out: most scroll events do not move the window by a row.
    if (start === this.startRow) return;
    this.startRow = start;

    this.body.style.transform = `translateY(${start * this.rowH}px)`;

    const frag = document.createDocumentFragment();
    const end = Math.min(this.totalRows, start + visible + 2);
    for (let r = start; r < end; r++) {
      const row = document.createElement("div");
      row.className = "row";
      for (let c = 0; c < this.cols; c++) {
        const cell = document.createElement("div");
        cell.className = c % 4 === 0 ? "cell num" : "cell";
        cell.textContent = this.cell(r, c);
        row.appendChild(cell);
      }
      frag.appendChild(row);
    }
    this.body.replaceChildren(frag);
  }
}
