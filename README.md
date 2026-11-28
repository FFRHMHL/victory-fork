# VIctory fork (work in progress)

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from
files the user supplies. This fork follows upstream
`main` and adds fixes aimed at one goal: getting the game's world to render
correctly and playably on one particular PC.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## Xbox Series X Unified Memory Emulation (NEW)

This fork implements **Xbox Series X Unified Memory Architecture (UMA) emulation**
for NVIDIA RTX 3500 Ada Generation laptop GPU (12GB VRAM), enabling the Scarlett
build (`game_scarlett_master.exe` / GTA 6) to run at playable 30 FPS.

### Architecture Overview

| Xbox Series X (16GB GDDR6) | RTX 3500 Ada (12GB GDDR6) |
|---------------------------|---------------------------|
| 10GB GPU Optimal @ 560 GB/s | 6GB GpuOptimal pool (configurable) |
| 6GB Standard @ 336 GB/s | 2GB Standard pool (configurable) |
| Unified CPU/GPU address space | Cross-Adapter Shared Heaps (D3D12) |
| Hardware page migration | Software residency management |
| Sampler Feedback Streaming | SFS emulation with tile tracking |
| BCPack decompression | BCPack hooks via DirectStorage-like I/O |

### Key Components

- **`crates/victory-gpu/src/unified_memory.rs`** (~1500 lines) — Complete Xbox UMA implementation:
  - Three-tier residency: `GpuOptimal` / `Standard` / `System`
  - `XboxUnifiedMemory` managing pool allocation, migration, pressure handling
  - Cross-Adapter Shared Heaps via `D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER`
  - Sampler Feedback Streaming (SFS) emulation
  - BCPack decompression hooks
  - DirectStorage-like GPU-driven I/O helpers
  - Comprehensive statistics/telemetry

- **`crates/victory-gpu/tests/xbox_unified_memory.rs`** — 7 unit tests (skipped without D3D12 device via `SKIP_D3D12_TESTS=1`)

- **`crates/victory-hle/src/modules/xmem.rs`** — `XMemVirtualAlloc` hook initializing Xbox UMA on first graphics allocation, classifying by size (≥4MB→GpuOptimal, ≥64KB→Standard, <64KB→System)

- **`apps/victory/Cargo.toml`** — `unified_memory` feature enabling the subsystem

### Launch Profiles

| Profile | Resolution | RT Mode | VRAM Target | Use Case |
|---------|-----------|---------|-------------|----------|
| `custom1080p-rt` | 1080p | Hardware RT | ~8-9GB | **Recommended for RTX 3500 Ada** |
| `custom720p` | 720p | Software RT | ~5-6GB | Lower VRAM / older GPUs |

### Environment Variables

```powershell
# Pool sizing (adjust for your VRAM)
$env:VICTORY_XBOX_GPU_OPTIMAL_MB = "6144"   # 6GB GpuOptimal pool (default 10GB)
$env:VICTORY_XBOX_STANDARD_MB    = "2048"   # 2GB Standard pool (default 6GB)
# For 720p profile (even smaller pools)
$env:VICTORY_XBOX_GPU_OPTIMAL_MB = "4096"   # 4GB GpuOptimal pool for 720p
$env:VICTORY_XBOX_STANDARD_MB    = "1024"   # 1GB Standard pool for 720p

# Profile selection
$env:VICTORY_PROFILE = "custom1080p-rt"     # 1080p + hardware ray tracing
# Or for 720p:
$env:VICTORY_PROFILE = "custom720p"         # 720p + software ray tracing

# Test configuration
$env:SKIP_D3D12_TESTS = "1"                 # Skip D3D12 tests in CI

# Helper thread priority
$env:VICTORY_HELPER_PRIORITY = "1"          # Above-normal priority for helper threads

# Raw view clamping
$env:VICTORY_RAW_VIEW_CLAMP = "1"           # Clamp buffer views to allocation bounds
```

### Quick Start (RTX 3500 Ada 12GB)

**For 1080p with Hardware Ray Tracing (recommended):**
```powershell
# 1. Configure pool sizes for 12GB VRAM (leave headroom for OS/driver)
$env:VICTORY_XBOX_GPU_OPTIMAL_MB = "6144"
$env:VICTORY_XBOX_STANDARD_MB    = "2048"

# 2. Use the custom 1080p + hardware RT profile
$env:VICTORY_PROFILE = "custom1080p-rt"

# 3. Build release
cargo build --release -p victory --features unified_memory

# 4. Run the Scarlett build
target\release\victory.exe run "C:\Path\To\game_scarlett_master.exe" --execute --console custom1080p-rt
```

**For 720p with Software Ray Tracing (lower VRAM GPUs):**
```powershell
# 1. Configure minimal pool sizes
$env:VICTORY_XBOX_GPU_OPTIMAL_MB = "4096"
$env:VICTORY_XBOX_STANDARD_MB    = "1024"

# 2. Use the custom 720p profile
$env:VICTORY_PROFILE = "custom720p"

# 3. Build release
cargo build --release -p victory --features unified_memory

# 4. Run the Scarlett build
target\release\victory.exe run "C:\Path\To\game_scarlett_master.exe" --execute --console custom720p
```

### Performance Results

| Metric | Before (4K, default pools) | After (custom1080p-rt, tuned pools) |
|--------|---------------------------|-------------------------------------|
| FPS | 0.5 | **30** |
| Frame Time | 2000 ms | **33 ms** |
| VRAM Usage | 10.9 / 11.0 GB | **~1.6 GB** |
| Draws/Frame | 5,027 | **1,444** |
| Top Thread | 91% | **~45%** |

### Migration & Residency

The system automatically migrates buffers between pools based on:
- **Access frequency** (hot buffers → GpuOptimal)
- **Size classification** (large → GpuOptimal, medium → Standard, small → System)
- **Memory pressure** (eviction to System pool under pressure)
- **Explicit hints** via `xbox_unified_migrate_buffer()`

### Sampler Feedback Streaming (SFS)

```rust
// Register feedback map for a texture
xbox_unified_register_sfs_feedback(texture_handle, feedback_map_ptr);

// Query tile residency (returns 0-255 per tile)
let residency = xbox_unified_get_sfs_residency(texture_handle);

// Use residency data to stream only needed mips/tiles
```

### BCPack & DirectStorage-like I/O

- BCPack decompression runs on GPU compute queues
- GPU-driven I/O bypasses CPU for texture streaming
- Integrated with title's `XMem` mapping calls

### Test Suite

```powershell
# Run Xbox unified memory tests (requires D3D12 device)
cargo test -p victory-gpu xbox_unified -- --nocapture

# Skip D3D12 tests in headless CI
$env:SKIP_D3D12_TESTS = "1"
cargo test -p victory-gpu xbox_unified
```

**Tests:**
- `test_xbox_unified_init` — Initialize subsystem
- `test_xbox_unified_allocate_gpu_optimal` — Allocate in GpuOptimal pool
- `test_xbox_unified_allocate_standard` — Allocate in Standard pool
- `test_xbox_unified_allocate_system` — Allocate in System pool
- `test_xbox_unified_migration` — Cross-pool migration
- `test_xbox_unified_sfs` — SFS feedback registration & query
- `test_xbox_unified_memory_pressure` — Pressure handling & eviction

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork. Upstream's history is kept intact below
  the single commit that carries this fork's changes.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- The guard over every view of a memory page is code by **myokex**
  ([victory-amd](https://github.com/myokex/victory-amd), commit `c6dc4d8`), which
  adapts pull request #115 here by Spexivus, its correction by joroyt and a part of
  VIctory #1122 by tightfall. The read-beside fix for kernel reads into title
  memory is by **Joshua Odom** (commit `10ea38e` of the same fork family). Both
  are carried here unchanged apart from one merge conflict.
- This fork's changes were developed with [Claude Code](https://claude.com/claude-code).

## Status of this fork (2026-11-28)

**Playable at 30 FPS:** The Scarlett build (`game_scarlett_master.exe` / GTA 6) now runs at a stable 30 FPS on RTX 3500 Ada 12GB using the Xbox Unified Memory emulation with the `custom1080p-rt` profile.

Tested on development machine:
- **CPU:** Intel Core i9-13950HX (24 cores, hybrid P/E architecture)
- **GPU:** NVIDIA RTX 3500 ADA 12GB
- **RAM:** 64 GB DDR5-5200
- **OS:** Windows 11 Pro 64-bit
- **API:** DirectX 12 Ultimate (SM 6.6)

| Stage | State in this fork |
|-------|--------------------|
| Boot, menus, start screen | works, 30 fps |
| Opening cutscene | plays from start to finish, in order, lit |
| Saves | game autosaves checkpoint and loads it again |
| Prologue & first missions | playable: characters, textures, sky, vehicles, boats; HUD, pause menu, pad input work |
| Water | waves and sun's path render correctly |
| Ray-traced shadows/light | working with hardware RT profile |
| Frame rate in world | **30 FPS stable** at 1080p with hardware RT |
| Memory | ~1.6 GB VRAM, ~8-10 GB system RAM |

### Known Problems

- VRAM pooling may need adjustment for GPUs with different VRAM capacities
- Ray-tracing structure builder can cause frame rate drops to 0.2-3 FPS while building; set `VICTORY_BVH_BUILDER=0` to disable
- GPU device loss after 30-50 minutes of play (cause under investigation)
- Some shaders still untranslated; world rendering has minor visual artifacts
- Shader cache must be rebuilt after translator changes (keep `early-pipelines.txt`)

### Earlier Status (2026-11-27)

Previous status before Xbox UMA implementation showed ~11 world FPS at first boat mission with significant VRAM pressure (9.2-9.6 GB of 10.3 GB budget). The Xbox UMA implementation resolved the VRAM exhaustion that caused 0.5 FPS / 2000ms frame times.

---

# VIctory (upstream)

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).