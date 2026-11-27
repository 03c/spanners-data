//! M-1 shell spike: Tauri host.
//!
//! Prints the same measurement contract as the egui spike so `bench/measure.sh`
//! can compare them without special-casing.
//!
//! `SPANNERS_READY` is emitted when the *frontend* reports its first painted
//! grid, not when the window is created — a blank window is not a usable app,
//! and counting it would flatter the shell.

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

fn flush() {
    let _ = std::io::stdout().flush();
}

#[tauri::command]
fn report_ready() {
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    println!("SPANNERS_READY {elapsed}");
    flush();
}

#[tauri::command]
fn report_frame(label: String, p95: f64, frames: u32) {
    let label: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    println!("SPANNERS_FRAME_P95_{label} {p95:.2} {frames}");
    flush();
}

#[tauri::command]
fn report_done(app: tauri::AppHandle) {
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(kb) = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            // Main process only; the harness sums the webview process tree.
            println!("SPANNERS_RSS_SELF_KB {kb}");
        }
    }
    flush();
    // Give the pipe a moment to drain before the process disappears.
    std::thread::sleep(std::time::Duration::from_millis(150));
    app.exit(0);
}

fn main() {
    START.set(Instant::now()).ok();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            report_ready,
            report_frame,
            report_done
        ])
        .run(tauri::generate_context!())
        .expect("error while running the shell spike");
}
