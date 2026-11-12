//! M-1 shell spike: pure-Rust (egui) side.
//!
//! Deliberately runs itself: no interaction needed. It renders the two screens
//! that decide the shell question — a 20k-line SQL buffer and a 100k-row grid —
//! then measures its own frame times and exits.
//!
//! Output contract (parsed by `bench/measure.sh`):
//!   SPANNERS_READY <ms since process start>
//!   SPANNERS_FRAME_P95_GRID <ms> <frames>
//!   SPANNERS_FRAME_P95_EDITOR <ms> <frames>
//!   SPANNERS_RSS_KB <kb>
//!
//! The editor is a plain `TextEdit`: egui has no mature syntax-highlighting
//! editor, and pretending otherwise would flatter the spike. The measurement it
//! produces — full re-layout of a 400 KB buffer per keystroke — is the honest
//! cost of the pure-Rust path.

use std::time::Instant;

use eframe::egui;

const ROWS: usize = 100_000;
const COLS: usize = 12;
const ROW_H: f32 = 22.0;
const POOL: usize = 512;
const SQL_LINES: usize = 20_000;

const WARMUP_FRAMES: u64 = 30;
const SCROLL_FRAMES: u64 = 180;
const TYPE_FRAMES: u64 = 30;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    Warmup,
    Scrolling,
    Typing,
    Done,
}

fn main() -> eframe::Result {
    let start = Instant::now();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_title("Spanners Data — shell spike (egui)"),
        ..Default::default()
    };

    eframe::run_native(
        "spanners-spike-egui",
        options,
        Box::new(move |_cc| Ok(Box::new(Spike::new(start)))),
    )
}

struct Spike {
    start: Instant,
    last_frame: Instant,
    frame: u64,
    phase: Phase,

    // Shared string pool: generating 1.2M Strings would measure the allocator,
    // not the renderer.
    pool: Vec<String>,
    sql: String,
    offset: f32,
    samples: Vec<f32>,
}

impl Spike {
    fn new(start: Instant) -> Self {
        let pool = (0..POOL)
            .map(|i| match i % 5 {
                0 => format!("{:08}", i * 37),
                1 => format!("customer-{}@example.com", i),
                2 => "2024-03-11 09:14:02".to_string(),
                3 => format!("{:.2}", i as f64 * 1.25),
                _ => format!("lorem ipsum dolor sit amet {}", i),
            })
            .collect();

        let mut sql = String::with_capacity(SQL_LINES * 48);
        for i in 0..SQL_LINES {
            sql.push_str(&format!(
                "SELECT id, name, created_at FROM table_{} WHERE tenant_id = {};\n",
                i % 500,
                i
            ));
        }

        Self {
            start,
            last_frame: Instant::now(),
            frame: 0,
            phase: Phase::Warmup,
            pool,
            sql,
            offset: 0.0,
            samples: Vec::with_capacity(SCROLL_FRAMES as usize),
        }
    }

    fn cell(&self, row: usize, col: usize) -> &str {
        &self.pool[(row * 31 + col * 17) % POOL]
    }

    fn measure(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32() * 1000.0;
        self.last_frame = now;
        self.frame += 1;

        if self.frame == 1 {
            println!("SPANNERS_READY {}", self.start.elapsed().as_millis());
        }

        match self.phase {
            Phase::Warmup if self.frame >= WARMUP_FRAMES => {
                self.phase = Phase::Scrolling;
                self.samples.clear();
                self.last_frame = Instant::now();
            }
            Phase::Scrolling => {
                self.samples.push(dt);
                self.offset += 48.0;
                if self.samples.len() as u64 >= SCROLL_FRAMES {
                    print_p95("GRID", &self.samples);
                    self.phase = Phase::Typing;
                    self.samples.clear();
                    self.last_frame = Instant::now();
                }
            }
            Phase::Typing => {
                self.samples.push(dt);
                // Worst case: appending forces a full re-layout of the buffer.
                self.sql.push('x');
                if self.samples.len() as u64 >= TYPE_FRAMES {
                    print_p95("EDITOR", &self.samples);
                    self.phase = Phase::Done;
                    print_rss();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            _ => {}
        }

        ctx.request_repaint();
    }
}

impl eframe::App for Spike {
    // egui 0.37 replaced `update(ctx)` with `ui(ui)`; panels are built inside
    // the root Ui rather than against the Context.
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.measure(&ctx);

        egui::Panel::top("editor")
            .resizable(true)
            .default_size(260.0)
            .show(root, |ui| {
                ui.label("SQL — 20,000 lines");
                egui::ScrollArea::vertical()
                    .id_salt("sql-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.sql)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12),
                        );
                    });
            });

        egui::CentralPanel::default().show(root, |ui| {
            ui.label(format!("{ROWS} rows · {COLS} columns"));
            ui.horizontal(|ui| {
                for c in 0..COLS {
                    ui.add_sized(
                        [110.0, ROW_H],
                        egui::Label::new(egui::RichText::new(format!("col_{c}")).strong())
                            .truncate(),
                    );
                }
            });
            ui.separator();

            let mut scroll = egui::ScrollArea::vertical()
                .id_salt("grid-scroll")
                .auto_shrink([false, false]);
            if self.phase == Phase::Scrolling {
                scroll = scroll.vertical_scroll_offset(self.offset);
            }

            scroll.show_rows(ui, ROW_H, ROWS, |ui, range| {
                for r in range {
                    ui.horizontal(|ui| {
                        for c in 0..COLS {
                            ui.add_sized(
                                [110.0, ROW_H],
                                egui::Label::new(self.cell(r, c)).truncate(),
                            );
                        }
                    });
                }
            });
        });
    }
}

fn print_p95(label: &str, samples: &[f32]) {
    if samples.is_empty() {
        return;
    }
    let mut sorted: Vec<f32> = samples.to_vec();
    sorted.sort_by(f32::total_cmp);
    let idx = ((sorted.len() as f32 * 0.95).ceil() as usize).saturating_sub(1);
    println!(
        "SPANNERS_FRAME_P95_{label} {:.2} {}",
        sorted[idx],
        sorted.len()
    );
}

fn print_rss() {
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(kb) = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            println!("SPANNERS_RSS_KB {kb}");
            return;
        }
    }
    println!("SPANNERS_RSS_KB 0");
}
