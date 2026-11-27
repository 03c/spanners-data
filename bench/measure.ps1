# Runs both M-1 spikes on Windows and prints the same markdown table as
# bench/measure.sh.
#
#   powershell -ExecutionPolicy Bypass -File bench\measure.ps1
#
# Build first:
#   cargo build --release --manifest-path apps/spike-egui/Cargo.toml
#   cd apps/spike-tauri; pnpm install; pnpm build
#   cargo build --release --manifest-path apps/spike-tauri/src-tauri/Cargo.toml

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$egui  = Join-Path $root "apps\spike-egui\target\release\spike-egui.exe"
$tauri = Join-Path $root "apps\spike-tauri\src-tauri\target\release\spike-tauri.exe"

function Get-TreeRss {
    param([int]$ProcId)
    $total = 0
    $p = Get-Process -Id $ProcId -ErrorAction SilentlyContinue
    if ($p) { $total += $p.WorkingSet64 }
    $children = Get-CimInstance Win32_Process -Filter "ParentProcessId=$ProcId" -ErrorAction SilentlyContinue
    foreach ($c in $children) { $total += Get-TreeRss -ProcId $c.ProcessId }
    return $total
}

function Measure-Spike {
    param([string]$Name, [string]$Path)

    if (-not (Test-Path $Path)) { throw "missing binary: $Path" }

    $out = New-TemporaryFile
    $proc = Start-Process -FilePath $Path -RedirectStandardOutput $out -PassThru -WindowStyle Minimized

    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Content $out -Raw -ErrorAction SilentlyContinue) -match "SPANNERS_READY") { break }
        if ($proc.HasExited) { break }
        Start-Sleep -Milliseconds 100
    }

    Start-Sleep -Seconds 1
    $rss = 0
    try { $rss = Get-TreeRss -ProcId $proc.Id } catch { }

    $proc.WaitForExit(120000) | Out-Null
    $text = Get-Content $out -Raw -ErrorAction SilentlyContinue

    $ready  = if ($text -match "SPANNERS_READY (\d+)")          { $Matches[1] } else { "n/a" }
    $grid   = if ($text -match "SPANNERS_FRAME_P95_GRID ([\d.]+)")   { $Matches[1] } else { "n/a" }
    $editor = if ($text -match "SPANNERS_FRAME_P95_EDITOR ([\d.]+)") { $Matches[1] } else { "n/a" }
    $rssMb  = [math]::Round($rss / 1MB)

    Remove-Item $out -Force -ErrorAction SilentlyContinue
    "| $Name | $ready ms | $grid ms | $editor ms | $rssMb MB |"
}

""
"| Shell | Cold start (first painted grid) | Grid p95 frame | Editor p95 frame | Peak RSS (tree) |"
"| --- | --- | --- | --- | --- |"
Measure-Spike -Name "Tauri + CodeMirror" -Path $tauri
Measure-Spike -Name "egui" -Path $egui
""
"Pass bar (PLAN.md §3.2): Tauri needs <=700 ms start, <=16.7 ms p95, <=200 MB."
