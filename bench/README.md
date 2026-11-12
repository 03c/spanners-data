# M-1 shell spike

Two throwaway apps that render the same two screens — a 20,000-line SQL buffer
and a 100,000-row virtual grid — and measure themselves. The winner becomes the
shell; the loser gets deleted.

## Run

```bash
# one-time, per shell
cargo build --release --manifest-path apps/spike-egui/Cargo.toml
(cd apps/spike-tauri && pnpm install && pnpm build)
cargo build --release --manifest-path apps/spike-tauri/src-tauri/Cargo.toml

# Linux (headless is fine, uses xvfb-run if DISPLAY is unset)
bench/measure.sh
```

Windows: run each `target\release\*.exe` from a terminal and read the
`SPANNERS_*` lines; `/proc` is not available, so use Task Manager (or
`Get-Process`) for RSS.

## Metrics

| Metric | Why it matters |
| --- | --- |
| `SPANNERS_READY` | process start → first painted grid, not first window |
| `SPANNERS_FRAME_P95_GRID` | 210 scripted scroll steps at 48 px/frame |
| `SPANNERS_FRAME_P95_EDITOR` | 60 appends at the end of a 20k-line buffer |
| tree RSS | process + WebKit subprocesses where applicable |

Both apps auto-exit when done; no interaction is needed. The rollout is
deterministic so runs are comparable between machines.

## Honest caveats

- The egui editor is a plain `TextEdit`, not a syntax-highlighting editor. The
  cost of *building* that editor (~8–12 weeks) is not in these numbers, and it is
  the main argument against the pure-Rust path.
- The Tauri frontend is vanilla TS, not Svelte. Svelte adds ~10 KB gzipped and a
  negligible amount of startup time; the difference does not move a 700 ms bar.
- Frame times include the X server / compositor. Compare the two shells on the
  same machine, not against these numbers from someone else's.
