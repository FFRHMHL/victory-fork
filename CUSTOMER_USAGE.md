# Custom Console Profile Usage

## Overview

The `PCx` profile runs GTA6 with:
- **Resolution**: 1080p (1920x1080) instead of 4K (3840x2160)
- **Ray Tracing**: Enabled on GPU (game detects as Series X device type)
- **Memory**: Series X configuration (10 GiB GPU-optimal + 3.5 GiB standard)

The `PCs` profile runs GTA6 with:
- **Resolution**: 720p (1280x720) instead of 4K (3840x2160)
- **Ray Tracing**: Disabled (game detects as Series S device type)
- **Memory**: Series S configuration (7,776 MiB standard)

## Usage

```bash
# PCx: 1080p with RT on GPU
victory run --console p-cx <path-to-game.exe>

# PCs: 720p with RT off
victory run --console p-cs <path-to-game.exe>
```

## Available Console Profiles

```
--console series-x      # Default: 4K resolution, RT on GPU, Series X memory
--console series-s      # Series S: Dynamic resolution, RT on GPU (when available), Series S memory
--console p-cx          # PCx: 1080p fixed, RT on GPU, Series X memory
--console p-cs          # PCs: 720p fixed, RT off, Series S memory
```

## P/E Core Thread Pinning

The victory-cpu crate provides infrastructure for P/E core management on Intel hybrid architectures:

```rust
use victory_cpu::{WorkType, ThreadPool, CoreAffinity};

// Heavy compute on P-cores (shader translation, pipeline compilation)
ThreadPool::spawn(WorkType::HeavyCompute, || {
    // GPU-intensive work
});

// I/O on E-cores (file watching, memory management)
ThreadPool::spawn(WorkType::Io, || {
    // Light background tasks
});

// GPU submission on P-cores
ThreadPool::spawn(WorkType::GpuSubmission, || {
    // Command submission
});
```

## Thread Pool API

```rust
// Get system core affinity
let affinity = CoreAffinity::default();
println!("P-cores: {}, E-cores: {}", 
    affinity.p_cores.count_ones(), 
    affinity.e_cores.count_ones()
);

// Work distributor for resource management
let distributor = WorkDistributor::new();

// Acquire slots for work
if distributor.try_acquire_p_core().is_ok() {
    // Do heavy work
    distributor.release_p_core();
}
```

## Offload Manager

```rust
let offload = OffloadManager::new();

// Check if GPU offload is possible
if offload.can_offload_to_gpu(WorkType::HeavyCompute) {
    // Offload computation to GPU
}
```