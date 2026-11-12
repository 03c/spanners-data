/**
 * M-1 shell spike: Tauri side.
 *
 * Same three screens and the same self-measuring script as the egui spike:
 * 20k-line SQL editor, 100k-row virtual grid, scripted scroll then scripted
 * typing. Frame deltas are collected with `requestAnimationFrame`, so a dropped
 * frame shows up as a >16.7 ms sample rather than being smoothed away.
 */
import { EditorView, basicSetup } from "codemirror";
import { sql, PostgreSQL } from "@codemirror/lang-sql";
import { invoke } from "@tauri-apps/api/core";
import { VirtualGrid } from "./grid";

const ROWS = 100_000;
const COLS = 12;
const POOL = 512;
const SQL_LINES = 20_000;

const WARMUP_FRAMES = 30;
const SCROLL_FRAMES = 210;
const TYPE_FRAMES = 60;

const pool = Array.from({ length: POOL }, (_, i) => {
  switch (i % 5) {
    case 0:
      return String(i * 37).padStart(8, "0");
    case 1:
      return `customer-${i}@example.com`;
    case 2:
      return "2024-03-11 09:14:02";
    case 3:
      return (i * 1.25).toFixed(2);
    default:
      return `lorem ipsum dolor sit amet ${i}`;
  }
});

const cell = (row: number, col: number) => pool[(row * 31 + col * 17) % POOL];

const head = document.getElementById("grid-head")!;
for (let c = 0; c < COLS; c++) {
  const d = document.createElement("div");
  d.textContent = `col_${c}`;
  head.appendChild(d);
}

const grid = new VirtualGrid(
  document.getElementById("grid-scroll")!,
  document.getElementById("grid-body")!,
  document.getElementById("grid-spacer")!,
  ROWS,
  COLS,
  cell,
);

let doc = "";
for (let i = 0; i < SQL_LINES; i++) {
  doc += `SELECT id, name, created_at FROM table_${i % 500} WHERE tenant_id = ${i};\n`;
}

const editor = new EditorView({
  doc,
  extensions: [basicSetup, sql({ dialect: PostgreSQL })],
  parent: document.getElementById("editor")!,
});

const status = document.getElementById("status")!;

const nextFrame = () =>
  new Promise<number>((resolve) => requestAnimationFrame(resolve));

function p95(samples: number[]): number {
  const sorted = [...samples].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * 0.95) - 1)];
}

async function measure(): Promise<void> {
  // Two frames: one to lay out, one to paint.
  await nextFrame();
  await nextFrame();
  await invoke("report_ready");

  for (let i = 0; i < WARMUP_FRAMES; i++) await nextFrame();

  const gridSamples: number[] = [];
  let last = performance.now();
  let offset = 0;
  for (let i = 0; i < SCROLL_FRAMES; i++) {
    const t = await nextFrame();
    gridSamples.push(t - last);
    last = t;
    offset += 48;
    grid.scrollTo(offset);
  }

  const editorSamples: number[] = [];
  last = performance.now();
  for (let i = 0; i < TYPE_FRAMES; i++) {
    const t = await nextFrame();
    editorSamples.push(t - last);
    last = t;
    editor.dispatch({ changes: { from: editor.state.doc.length, insert: "x" } });
  }

  status.textContent = `grid p95 ${p95(gridSamples).toFixed(2)} ms · editor p95 ${p95(editorSamples).toFixed(2)} ms`;

  await invoke("report_frame", {
    label: "GRID",
    p95: p95(gridSamples),
    frames: gridSamples.length,
  });
  await invoke("report_frame", {
    label: "EDITOR",
    p95: p95(editorSamples),
    frames: editorSamples.length,
  });
  await invoke("report_done");
}

void measure();
