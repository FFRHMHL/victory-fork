//! Unified Memory for APU ↔ dGPU sharing on PC — Xbox Series X emulation layer.
//!
//! On a PC with a discrete GPU (like NVIDIA RTX 3500 ADA 12GB) and no unified
//! memory, this module emulates the Xbox Series X's unified memory architecture
//! using D3D12 cross-adapter heaps and explicit residency management.
//!
//! Xbox Series X Memory Architecture:
//! - 16 GB GDDR6 unified memory on 320-bit bus
//! - 10 GB "GPU Optimal Memory" @ 560 GB/s (14 Gbps modules)
//! - 6 GB "Standard Memory" @ 336 GB/s
//! - 13.5 GB developer accessible (10 GB GPU-optimal + 3.5 GB standard)
//! - 5 MB GPU L2 cache
//!
//! Xbox Velocity Architecture:
//! - Sampler Feedback Streaming (SFS): hardware texture streaming loading only needed mips
//! - DirectStorage: GPU-driven I/O with hardware decompression (LZ + BCPack)
//! - BCPack: custom texture compression decompression on GPU
//!
//! RTX 3500 ADA 12GB Target:
//! - 12 GB GDDR6 VRAM @ 432 GB/s (192-bit bus, 18 Gbps)
//! - PCIe 4.0 x16 (~32 GB/s bidirectional)
//! - 5,120 CUDA cores, 160 Tensor cores (4th gen), 40 RT cores (3rd gen)
//! - Ada Lovelace architecture (TSMC 4N/5nm)
//!
//! Emulation Strategy:
//! 1. GPU Optimal Pool (8 GB VRAM): High-frequency textures, render targets, UAV buffers
//!    - Mapped directly in VRAM, accessed at 432 GB/s
//! 2. Standard Pool (4 GB VRAM): Less frequent textures, staging buffers
//!    - Can spill to system RAM via cross-adapter heaps over PCIe 4.0
//! 3. System Memory Pool (unlimited): CPU-only data, large buffers
//!    - Cross-adapter heaps with write-back CPU pages (L0 pool)
//!
//! Residency Tracking:
//! - GPUOptimal: Data in VRAM, accessed at full VRAM bandwidth
//! - Standard: Data in VRAM or system RAM, migrated based on usage
//! - System: Data in system RAM, CPU/APU accessible
//! - InFlight: Data being transferred between pools

#[cfg(feature = "unified_memory")]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::core::Result as WinResult;

const GENERIC_ALL: u32 = 0x10000000;

use crate::host::{host, HostList};

/// Xbox Series X memory pool classification for emulation on discrete GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XboxMemoryPool {
    /// GPU Optimal Memory: 10 GB @ 560 GB/s on Xbox → 8 GB VRAM @ 432 GB/s on RTX 3500 ADA
    /// High-frequency access: render targets, UAV buffers, active texture mips
    GpuOptimal,
    /// Standard Memory: 6 GB @ 336 GB/s on Xbox → 4 GB VRAM @ 432 GB/s (or system RAM over PCIe)
    /// Less frequent access: texture mips, streaming buffers, staging
    Standard,
    /// System Memory: Unlimited system RAM over PCIe 4.0 x16 (~32 GB/s)
    /// CPU/APU only: large buffers, audio, infrequent data
    System,
}

/// Where the authoritative copy of the buffer's data currently resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Data is in VRAM (GPU Optimal pool) — full 432 GB/s bandwidth
    GpuOptimal,
    /// Data is in VRAM (Standard pool) or system RAM — migrated based on usage
    Standard,
    /// Data is in system RAM (CPU/APU wrote it last) — PCIe bandwidth (~32 GB/s)
    System,
    /// Data is being transferred between pools; both sides may have stale copies
    InFlight,
}

/// Configuration for Xbox memory emulation on RTX 3500 ADA 12GB
#[derive(Debug, Clone)]
pub struct XboxMemoryConfig {
    /// VRAM budget for GPU Optimal pool (default: 8 GB)
    pub gpu_optimal_vram_mb: u64,
    /// VRAM budget for Standard pool (default: 4 GB)
    pub standard_vram_mb: u64,
    /// Enable Sampler Feedback Streaming emulation (texture mip streaming)
    pub enable_sfs_emulation: bool,
    /// Enable BCPack texture decompression on GPU
    pub enable_bcpack_decompression: bool,
    /// Enable DirectStorage-like GPU-driven I/O
    pub enable_gpu_driven_io: bool,
    /// PCIe 4.0 x16 bandwidth estimate in GB/s (default: 32)
    pub pcie_bandwidth_gbps: u64,
    /// VRAM bandwidth in GB/s (RTX 3500 ADA: 432)
    pub vram_bandwidth_gbps: u64,
}

impl Default for XboxMemoryConfig {
    fn default() -> Self {
        Self {
            // RTX 3500 ADA 12GB: reserve 8 GB for GPU Optimal, 4 GB for Standard
            gpu_optimal_vram_mb: 8192,
            standard_vram_mb: 4096,
            enable_sfs_emulation: true,
            enable_bcpack_decompression: true,
            enable_gpu_driven_io: true,
            pcie_bandwidth_gbps: 32,
            vram_bandwidth_gbps: 432,
        }
    }
}

impl XboxMemoryConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let mb = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default) << 20
        };

        Self {
            gpu_optimal_vram_mb: mb("VICTORY_XBOX_GPU_OPTIMAL_MB", 8192),
            standard_vram_mb: mb("VICTORY_XBOX_STANDARD_MB", 4096),
            enable_sfs_emulation: std::env::var_os("VICTORY_XBOX_SFS").is_none_or(|v| v != "0"),
            enable_bcpack_decompression: std::env::var_os("VICTORY_XBOX_BCPACK").is_none_or(|v| v != "0"),
            enable_gpu_driven_io: std::env::var_os("VICTORY_XBOX_GPU_IO").is_none_or(|v| v != "0"),
            pcie_bandwidth_gbps: std::env::var("VICTORY_XBOX_PCIE_GBPS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(32),
            vram_bandwidth_gbps: std::env::var("VICTORY_XBOX_VRAM_GBPS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(432),
        }
    }

    /// Get total VRAM budget
    pub fn total_vram_mb(&self) -> u64 {
        self.gpu_optimal_vram_mb + self.standard_vram_mb
    }
}

/// Global Xbox memory configuration
static XBOX_CONFIG: OnceLock<XboxMemoryConfig> = OnceLock::new();

/// Get or initialize the global Xbox memory configuration
fn xbox_config() -> &'static XboxMemoryConfig {
    XBOX_CONFIG.get_or_init(XboxMemoryConfig::from_env)
}

/// Statistics for the Xbox unified memory system.
static XBOX_STATS: [AtomicU64; 16] = [
    const { AtomicU64::new(0) }, // GPU Optimal allocations
    const { AtomicU64::new(0) }, // Standard allocations
    const { AtomicU64::new(0) }, // System allocations
    const { AtomicU64::new(0) }, // GPU Optimal bytes current
    const { AtomicU64::new(0) }, // Standard bytes current
    const { AtomicU64::new(0) }, // System bytes current
    const { AtomicU64::new(0) }, // GPU Optimal → Standard migrations
    const { AtomicU64::new(0) }, // Standard → GPU Optimal migrations
    const { AtomicU64::new(0) }, // Standard → System migrations
    const { AtomicU64::new(0) }, // System → Standard migrations
    const { AtomicU64::new(0) }, // SFS texture streaming requests
    const { AtomicU64::new(0) }, // SFS mips loaded
    const { AtomicU64::new(0) }, // BCPack decompressions
    const { AtomicU64::new(0) }, // GPU I/O requests
    const { AtomicU64::new(0) }, // PCIe transfers (bytes)
    const { AtomicU64::new(0) }, // Residency changes
];

/// A buffer placed in the Xbox emulated unified memory system.
#[derive(Debug)]
struct XboxUnifiedBuffer {
    /// Buffer on the host (APU) device — in system RAM
    apu_buffer: ID3D12Resource,
    /// Buffer on the dGPU device (opened via OpenSharedHandle) — in VRAM
    dgpu_buffer: Option<ID3D12Resource>,
    /// Offset within the heap
    offset: u64,
    /// Size in bytes
    size: u64,
    /// CPU mapping pointer (write-back cached), None for VRAM-only buffers
    cpu_mapping: Option<CpuPtr>,
    /// GPU virtual address on APU device
    apu_gpu_va: u64,
    /// GPU virtual address on dGPU device
    dgpu_gpu_va: Option<u64>,
    /// Current residency: where the valid data lives (Xbox unified memory model)
    residency: Residency,
    /// Target memory pool for this buffer
    target_pool: XboxMemoryPool,
    /// Fence for APU→dGPU synchronization
    apu_to_dgpu_fence: Option<ID3D12Fence>,
    /// Fence for dGPU→APU synchronization
    dgpu_to_apu_fence: Option<ID3D12Fence>,
    /// Sampler Feedback Streaming data (for texture streaming)
    sfs_feedback: Option<SfsFeedback>,
    /// Access frequency hint (for migration decisions)
    access_frequency: u32,
    /// Last frame this buffer was accessed
    last_access_frame: u64,
}

/// Sampler Feedback Streaming feedback data for texture streaming emulation
#[derive(Debug, Clone)]
struct SfsFeedback {
    /// Which mip levels are resident (bitmask)
    resident_mips: u32,
    /// Which mip levels were requested this frame
    requested_mips: u32,
    /// Total texture size in bytes
    total_size: u64,
    /// Size of each mip level
    mip_sizes: Vec<u64>,
}

/// A handle to an Xbox unified buffer, returned to callers.
#[derive(Debug, Clone)]
pub struct XboxUnifiedBufferHandle {
    heap_id: u64,
    offset: u64,
}

impl XboxUnifiedBufferHandle {
    /// Get the CPU mapping pointer for APU/CPU access.
    pub fn cpu_ptr(&self) -> Option<*mut u8> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        buffer.cpu_mapping.as_ref().map(|p| p.as_ptr())
    }

    /// Get the APU GPU virtual address.
    pub fn apu_gpu_va(&self) -> Option<u64> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        Some(buffer.apu_gpu_va)
    }

    /// Get the dGPU GPU virtual address (if dGPU device is attached).
    pub fn dgpu_gpu_va(&self) -> Option<u64> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        buffer.dgpu_gpu_va
    }

    /// Get the buffer size.
    pub fn size(&self) -> Option<u64> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        Some(buffer.size)
    }

    /// Current residency of the buffer's data.
    pub fn residency(&self) -> Option<Residency> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        Some(buffer.residency)
    }

    /// Target memory pool for this buffer.
    pub fn target_pool(&self) -> Option<XboxMemoryPool> {
        let unified = XBOX_UNIFIED.get()?.lock().ok()?;
        let buffer = unified.buffers.get(&self.heap_id)?;
        Some(buffer.target_pool)
    }

    /// Migrate buffer data to GPU Optimal pool (VRAM).
    /// This issues a copy on the dGPU queue from the shared heap buffer
    /// to a dGPU-local buffer in VRAM, then updates residency.
    pub fn migrate_to_gpu_optimal(&self, _list: &HostList) -> WinResult<()> {
        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();

        let config = xbox_config();
        let current_gpu_optimal = unified.gpu_optimal_bytes.load(Ordering::Relaxed);

        // Check if we need to evict before borrowing buffer mutably
        let (needs_eviction, buffer_size, is_gpu_optimal, has_dgpu_buffer) = {
            let buffer = unified.buffers.get(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            let size = buffer.size;
            let residency = buffer.residency;
            let has_dgpu = buffer.dgpu_buffer.is_some();
            (current_gpu_optimal + size > config.gpu_optimal_vram_mb * 1024 * 1024, size, residency == Residency::GpuOptimal, has_dgpu)
        };

        if is_gpu_optimal {
            return Ok(());
        }
        if !has_dgpu_buffer {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        if needs_eviction {
            unified.evict_from_gpu_optimal(buffer_size)?;
        }

        let buffer_size = {
            let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            buffer.residency = Residency::InFlight;
            buffer.target_pool = XboxMemoryPool::GpuOptimal;
            buffer.size
        };
        XBOX_STATS[6].fetch_add(1, Ordering::Relaxed); // Standard → GPU Optimal
        XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
        unified.gpu_optimal_bytes.fetch_add(buffer_size, Ordering::Relaxed);

        Ok(())
    }

    /// Migrate buffer data to Standard pool (VRAM or system RAM based on usage).
    pub fn migrate_to_standard(&self, _list: &HostList) -> WinResult<()> {
        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();

        let (buffer_size, needs_migration) = {
            let buffer = unified.buffers.get(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            let size = buffer.size;
            if buffer.residency == Residency::Standard {
                return Ok(());
            }
            if buffer.dgpu_buffer.is_none() {
                return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
            }
            (size, true)
        };

        if needs_migration {
            let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            buffer.residency = Residency::InFlight;
            buffer.target_pool = XboxMemoryPool::Standard;
            XBOX_STATS[7].fetch_add(1, Ordering::Relaxed); // GPU Optimal → Standard
            XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
            unified.standard_bytes.fetch_add(buffer_size, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Migrate buffer data to System pool (system RAM).
    pub fn migrate_to_system(&self, _list: &HostList) -> WinResult<()> {
        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();

        let (buffer_size, needs_migration) = {
            let buffer = unified.buffers.get(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            let size = buffer.size;
            if buffer.residency == Residency::System {
                return Ok(());
            }
            (size, true)
        };

        if needs_migration {
            let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            buffer.residency = Residency::InFlight;
            buffer.target_pool = XboxMemoryPool::System;
            XBOX_STATS[8].fetch_add(1, Ordering::Relaxed); // Standard → System
            XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
            unified.system_bytes.fetch_add(buffer_size, Ordering::Relaxed);
        }

        Ok(())
    }

    /// After a migration copy completes on the dGPU queue, call this to
    /// finalize the residency change and make the data visible on the APU side.
    pub fn finalize_migration(&self, target_residency: Residency) -> WinResult<()> {
        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        buffer.residency = target_residency;
        XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
        Ok(())
    }

    /// Record texture mip access for SFS emulation
    pub fn record_mip_access(&self, mip_level: u32) -> WinResult<()> {
        let config = xbox_config();
        if !config.enable_sfs_emulation {
            return Ok(());
        }

        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        if let Some(sfs) = &mut buffer.sfs_feedback {
            sfs.requested_mips |= 1 << mip_level;
            XBOX_STATS[10].fetch_add(1, Ordering::Relaxed); // SFS requests
        }

        Ok(())
    }

    /// Process SFS feedback and load required mips (call per frame)
    pub fn process_sfs_feedback(&self, list: &HostList) -> WinResult<()> {
        let config = xbox_config();
        if !config.enable_sfs_emulation {
            return Ok(());
        }

        let mut unified = XBOX_UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let buffer = unified.buffers.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        if let Some(sfs) = &mut buffer.sfs_feedback {
            let newly_requested = sfs.requested_mips & !sfs.resident_mips;
            if newly_requested != 0 {
                // Load newly requested mips to GPU Optimal
                for mip in 0..32 {
                    if newly_requested & (1 << mip) != 0 {
                        // In a real implementation, this would trigger a DirectStorage load
                        sfs.resident_mips |= 1 << mip;
                        XBOX_STATS[11].fetch_add(1, Ordering::Relaxed); // SFS mips loaded
                    }
                }
                sfs.requested_mips = 0;
            }
        }

        Ok(())
    }
}

/// Wrapper for raw pointer that is Send+Sync.
/// SAFETY: The CPU mapping is from a D3D12 heap with write-back caching,
/// accessed only through explicit synchronization (fences/barriers).
#[derive(Debug)]
struct CpuPtr(NonNull<u8>);

unsafe impl Send for CpuPtr {}
unsafe impl Sync for CpuPtr {}

impl CpuPtr {
    fn as_ptr(&self) -> *mut u8 {
        self.0.as_ptr()
    }
}

/// Wrapper for HANDLE that is Send+Sync.
/// SAFETY: The HANDLE is managed by the unified memory system and only
/// accessed through explicit synchronization.
#[derive(Debug)]
struct SafeHandle(HANDLE);

unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

impl SafeHandle {
    fn get(&self) -> HANDLE {
        self.0
    }
}

/// Global unified memory registry: cross-adapter heaps and their placed buffers.
/// The APU device (or CPU) writes, the dGPU device reads — or both can write
/// with explicit synchronization (fences, resource barriers).
static UNIFIED: OnceLock<Mutex<UnifiedMemory>> = OnceLock::new();

/// Statistics for the unified memory system.
static STATS: [AtomicU64; 8] = [
    const { AtomicU64::new(0) }, // heaps created
    const { AtomicU64::new(0) }, // heaps destroyed
    const { AtomicU64::new(0) }, // buffers placed
    const { AtomicU64::new(0) }, // buffers released
    const { AtomicU64::new(0) }, // bytes currently in unified heaps
    const { AtomicU64::new(0) }, // APU→dGPU migrations
    const { AtomicU64::new(0) }, // dGPU→APU migrations
    const { AtomicU64::new(0) }, // coherency flushes
];

/// A cross-adapter heap and its placed buffers, shared between the APU (host)
/// device and an external dGPU device.
#[derive(Debug)]
struct UnifiedHeap {
    /// The heap on the host (APU) device.
    heap: ID3D12Heap,
    /// Size of the heap in bytes.
    size: u64,
    /// The dGPU device this heap is shared with (if opened on dGPU).
    dgpu_device: Option<ID3D12Device>,
    /// The dGPU's handle to the same heap (opened via OpenSharedHandle).
    dgpu_heap: Option<ID3D12Heap>,
    /// NT handle for sharing with dGPU process/device.
    shared_handle: Option<SafeHandle>,
    /// Buffers placed in this heap, by offset.
    buffers: HashMap<u64, UnifiedBuffer>,
    /// Current allocation cursor (next free offset, 64 KiB aligned).
    cursor: u64,
}

/// A buffer placed in a unified heap, accessible from both APU and dGPU.
#[derive(Debug)]
struct UnifiedBuffer {
    /// Buffer on the host (APU) device.
    apu_buffer: ID3D12Resource,
    /// Buffer on the dGPU device (opened via OpenSharedHandle).
    dgpu_buffer: Option<ID3D12Resource>,
    /// Offset within the heap.
    offset: u64,
    /// Size in bytes.
    size: u64,
    /// CPU mapping pointer (write-back cached).
    cpu_mapping: CpuPtr,
    /// GPU virtual address on APU device.
    apu_gpu_va: u64,
    /// GPU virtual address on dGPU device.
    dgpu_gpu_va: Option<u64>,
    /// Current residency: where the valid data lives.
    residency: UnifiedResidency,
    /// Fence for APU→dGPU synchronization.
    apu_to_dgpu_fence: Option<ID3D12Fence>,
    /// Fence for dGPU→APU synchronization.
    dgpu_to_apu_fence: Option<ID3D12Fence>,
}

/// Where the authoritative copy of the buffer's data currently resides (cross-adapter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedResidency {
    /// Data is in system RAM (CPU/APU wrote it last).
    ApU,
    /// Data has been migrated to dGPU VRAM (dGPU wrote it last, or explicit migrate).
    Dgpu,
    /// Data is being transferred; both sides may have stale copies.
    InFlight,
}

/// A handle to a unified buffer, returned to callers.
#[derive(Debug, Clone)]
pub struct UnifiedBufferHandle {
    pub heap_id: u64,
    pub offset: u64,
}

impl UnifiedBufferHandle {
    /// Get the CPU mapping pointer for APU/CPU access.
    pub fn cpu_ptr(&self) -> Option<*mut u8> {
        let unified = UNIFIED.get()?.lock().ok()?;
        let heap = unified.heaps.get(&self.heap_id)?;
        let buffer = heap.buffers.get(&self.offset)?;
        Some(buffer.cpu_mapping.as_ptr())
    }

    /// Get the APU GPU virtual address.
    pub fn apu_gpu_va(&self) -> Option<u64> {
        let unified = UNIFIED.get()?.lock().ok()?;
        let heap = unified.heaps.get(&self.heap_id)?;
        let buffer = heap.buffers.get(&self.offset)?;
        Some(buffer.apu_gpu_va)
    }

    /// Get the dGPU GPU virtual address (if dGPU device is attached).
    pub fn dgpu_gpu_va(&self) -> Option<u64> {
        let unified = UNIFIED.get()?.lock().ok()?;
        let heap = unified.heaps.get(&self.heap_id)?;
        let buffer = heap.buffers.get(&self.offset)?;
        buffer.dgpu_gpu_va
    }

    /// Get the buffer size.
    pub fn size(&self) -> Option<u64> {
        let unified = UNIFIED.get()?.lock().ok()?;
        let heap = unified.heaps.get(&self.heap_id)?;
        let buffer = heap.buffers.get(&self.offset)?;
        Some(buffer.size)
    }

    /// Current residency of the buffer's data.
    pub fn residency(&self) -> Option<UnifiedResidency> {
        let unified = UNIFIED.get()?.lock().ok()?;
        let heap = unified.heaps.get(&self.heap_id)?;
        let buffer = heap.buffers.get(&self.offset)?;
        Some(buffer.residency)
    }

    /// Migrate buffer data from APU (system RAM) to dGPU.
    /// This issues a copy on the dGPU queue from the shared heap buffer
    /// to a dGPU-local buffer, then updates residency.
    pub fn migrate_to_dgpu(&self, list: &HostList) -> WinResult<()> {
        let mut unified = UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let heap = unified.heaps.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        let buffer = heap.buffers.get_mut(&self.offset).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        if buffer.residency == UnifiedResidency::Dgpu {
            return Ok(());
        }
        if buffer.dgpu_buffer.is_none() || heap.dgpu_device.is_none() {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        // Ensure APU writes are visible: flush CPU cache (write-back pages)
        // For write-back memory, the CPU cache is coherent with GPU on same device,
        // but for cross-adapter we need explicit synchronization.
        // The dGPU will read from the shared heap buffer directly.

        // Record a barrier on the APU side: transition to COPY_SOURCE
        // (Not strictly needed for heap buffers in COMMON state, but good practice)
        // list.resource_barrier(...);

        // On dGPU queue: copy from shared buffer to dGPU-local buffer
        // This requires a dGPU command list, which we don't have direct access to here.
        // Instead, we mark residency as InFlight and the caller must execute the copy.
        buffer.residency = UnifiedResidency::InFlight;
        STATS[5].fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Migrate buffer data from dGPU to APU (system RAM).
    pub fn migrate_to_apu(&self, list: &HostList) -> WinResult<()> {
        let mut unified = UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let heap = unified.heaps.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        let buffer = heap.buffers.get_mut(&self.offset).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        if buffer.residency == UnifiedResidency::ApU {
            return Ok(());
        }
        if buffer.dgpu_buffer.is_none() {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        // Copy from dGPU-local buffer to shared heap buffer on dGPU queue
        buffer.residency = UnifiedResidency::InFlight;
        STATS[6].fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// After a migration copy completes on the dGPU queue, call this to
    /// finalize the residency change and make the data visible on the APU side.
    pub fn finalize_migration(&self, to_dgpu: bool) -> WinResult<()> {
        let mut unified = UNIFIED.get().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?.lock().unwrap();
        let heap = unified.heaps.get_mut(&self.heap_id).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        let buffer = heap.buffers.get_mut(&self.offset).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        if to_dgpu {
            buffer.residency = UnifiedResidency::Dgpu;
        } else {
            buffer.residency = UnifiedResidency::ApU;
            // For APU residency: the shared heap buffer in system RAM now has the data.
            // CPU mapping is already coherent (write-back).
        }
        STATS[7].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The unified memory system: manages cross-adapter heaps and buffers.
struct UnifiedMemory {
    /// All unified heaps, by ID.
    heaps: HashMap<u64, UnifiedHeap>,
    /// Next heap ID.
    next_heap_id: u64,
    /// The APU (host) device.
    apu_device: ID3D12Device,
    /// The dGPU device (if attached).
    dgpu_device: Option<ID3D12Device>,
    /// The dGPU adapter LUID (for identifying the dGPU).
    dgpu_luid: Option<LUID>,
}

// SAFETY: D3D12 devices are free-threaded; they can be shared across threads.
// The unified memory system is only accessed through a Mutex.
unsafe impl Send for UnifiedMemory {}
unsafe impl Sync for UnifiedMemory {}

impl UnifiedMemory {
    /// Get or create the global unified memory system.
    fn get_or_init() -> std::sync::MutexGuard<'static, UnifiedMemory> {
        let host = host().expect("host device not initialized");
        UNIFIED.get_or_init(|| {
            Mutex::new(UnifiedMemory {
                heaps: HashMap::new(),
                next_heap_id: 1,
                apu_device: host.device.clone(),
                dgpu_device: None,
                dgpu_luid: None,
            })
        }).lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attach a dGPU device for cross-adapter sharing.
    /// The dGPU device must be created on a different adapter than the APU.
    pub fn attach_dgpu(device: ID3D12Device) -> WinResult<()> {
        let mut unified = Self::get_or_init();

        // Verify it's a different adapter
        let apu_luid = unsafe { unified.apu_device.GetAdapterLuid() };
        let dgpu_luid = unsafe { device.GetAdapterLuid() };
        if apu_luid.LowPart == dgpu_luid.LowPart && apu_luid.HighPart == dgpu_luid.HighPart {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        unified.dgpu_device = Some(device.clone());
        unified.dgpu_luid = Some(dgpu_luid);

        // Open existing heaps on the dGPU device
        for (_, heap) in unified.heaps.iter_mut() {
            heap.open_on_dgpu(&device)?;
        }

        tracing::info!(
            apu_luid = format_args!("{:#x}:{:#x}", apu_luid.HighPart, apu_luid.LowPart),
            dgpu_luid = format_args!("{:#x}:{:#x}", dgpu_luid.HighPart, dgpu_luid.LowPart),
            "unified memory: attached dGPU device"
        );
        Ok(())
    }

    /// Create a new unified heap of `size` bytes (64 KiB aligned).
    /// The heap is created on the APU device with cross-adapter sharing enabled.
    pub fn create_heap(size: u64) -> WinResult<UnifiedBufferHandle> {
        let mut unified = Self::get_or_init();
        let heap_id = unified.next_heap_id;
        unified.next_heap_id += 1;

        // Create a custom heap in system memory (L0 pool, write-back CPU pages)
        // with cross-adapter sharing flags.
        let heap_size = size.next_multiple_of(64 * 1024);
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            CreationNodeMask: 1, // APU node
            VisibleNodeMask: 0,  // visible to all (will be set per-buffer)
            ..Default::default()
        };

        let heap_flags = D3D12_HEAP_FLAG_SHARED
            | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER
            | D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES;

        let heap_desc = D3D12_HEAP_DESC {
            SizeInBytes: heap_size,
            Properties: heap_props,
            Alignment: 64 * 1024,
            Flags: heap_flags,
        };

        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            unified.apu_device.CreateHeap(&heap_desc, &mut heap)?;
        }
        let heap = heap.ok_or_else(windows::core::Error::empty)?;

        // Create NT handle for sharing with dGPU
        let mut shared_handle: Option<SafeHandle> = None;
        if let Some(dgpu_device) = &unified.dgpu_device {
            unsafe {
                let handle = unified.apu_device.CreateSharedHandle(
                    &heap,
                    None,
                    GENERIC_ALL,
                    None,
                )?;
                shared_handle = Some(SafeHandle(handle));
            }
        }

        let mut unified_heap = UnifiedHeap {
            heap,
            size: heap_size,
            dgpu_device: unified.dgpu_device.clone(),
            dgpu_heap: None,
            shared_handle,
            buffers: HashMap::new(),
            cursor: 0,
        };

        // If dGPU is attached, open the heap there too
        if let Some(dgpu_device) = &unified.dgpu_device {
            unified_heap.open_on_dgpu(dgpu_device)?;
        }

        unified.heaps.insert(heap_id, unified_heap);
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[4].fetch_add(heap_size, Ordering::Relaxed);

        // Return a dummy handle for the heap itself (offset 0, but size = heap size)
        Ok(UnifiedBufferHandle { heap_id, offset: 0 })
    }

    /// Allocate a buffer in a unified heap.
    /// Returns a handle for CPU/APU and GPU access.
    pub fn allocate_buffer(heap_handle: &UnifiedBufferHandle, size: u64, flags: D3D12_RESOURCE_FLAGS) -> WinResult<UnifiedBufferHandle> {
        let mut unified = Self::get_or_init();

        // Clone APU device before borrowing heap mutably
        let apu_device = unified.apu_device.clone();

        let heap = unified.heaps.get_mut(&heap_handle.heap_id)
            .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        let aligned_size = size.next_multiple_of(64 * 1024);
        let offset = heap.cursor.next_multiple_of(64 * 1024);
        if offset + aligned_size > heap.size {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY));
        }

        // Create buffer on APU device, placed in the unified heap
        let buffer_flags = flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER;
        let buffer_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: aligned_size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: buffer_flags,
            ..Default::default()
        };

        let mut apu_buffer: Option<ID3D12Resource> = None;
        unsafe {
            apu_device.CreatePlacedResource(
                &heap.heap,
                offset,
                &buffer_desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut apu_buffer,
            )?;
        }
        let apu_buffer = apu_buffer.ok_or_else(windows::core::Error::empty)?;

        // Map for CPU access (write-back cached)
        let mut cpu_mapping: *mut c_void = std::ptr::null_mut();
        unsafe {
            apu_buffer.Map(0, None, Some(&mut cpu_mapping))?;
        }
        let cpu_mapping_ptr = cpu_mapping.cast::<u8>();
        let cpu_mapping = CpuPtr(NonNull::new(cpu_mapping_ptr).unwrap());

        let apu_gpu_va = unsafe { apu_buffer.GetGPUVirtualAddress() };

        // Create buffer on dGPU if attached
        let dgpu_heap_device = heap.dgpu_device.clone();
        let (dgpu_buffer, dgpu_gpu_va) = if let Some(dgpu_device) = &dgpu_heap_device {
            let dgpu_heap = heap.dgpu_heap.as_ref()
                .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
            let mut dgpu_buf: Option<ID3D12Resource> = None;
            unsafe {
                dgpu_device.CreatePlacedResource(
                    dgpu_heap,
                    offset,
                    &buffer_desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut dgpu_buf,
                )?;
            }
            let dgpu_buf = dgpu_buf.ok_or_else(windows::core::Error::empty)?;
            let va = unsafe { dgpu_buf.GetGPUVirtualAddress() };
            (Some(dgpu_buf), Some(va))
        } else {
            (None, None)
        };

        // Create synchronization fences
        let mut apu_to_dgpu_fence: Option<ID3D12Fence> = None;
        let mut dgpu_to_apu_fence: Option<ID3D12Fence> = None;
        if dgpu_heap_device.is_some() {
            unsafe {
                let apu_fence: ID3D12Fence = apu_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
                apu_to_dgpu_fence = Some(apu_fence);
                let dgpu_device = dgpu_heap_device.as_ref().unwrap();
                let dgpu_fence: ID3D12Fence = dgpu_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
                dgpu_to_apu_fence = Some(dgpu_fence);
            }
        }

        let buffer = UnifiedBuffer {
            apu_buffer,
            dgpu_buffer,
            offset,
            size: aligned_size,
            cpu_mapping,
            apu_gpu_va,
            dgpu_gpu_va,
            residency: UnifiedResidency::ApU,
            apu_to_dgpu_fence,
            dgpu_to_apu_fence,
        };

        heap.buffers.insert(offset, buffer);
        heap.cursor = offset + aligned_size;
        STATS[2].fetch_add(1, Ordering::Relaxed);
        STATS[4].fetch_add(aligned_size, Ordering::Relaxed);

        Ok(UnifiedBufferHandle { heap_id: heap_handle.heap_id, offset })
    }

    /// Release a buffer back to the heap (offset becomes free for reuse).
    /// Note: this is a simple bump allocator; real impl would need a free list.
    pub fn release_buffer(handle: &UnifiedBufferHandle) -> WinResult<()> {
        let mut unified = Self::get_or_init();
        let heap = unified.heaps.get_mut(&handle.heap_id)
            .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        if heap.buffers.remove(&handle.offset).is_some() {
            STATS[3].fetch_add(1, Ordering::Relaxed);
            STATS[4].fetch_sub(1, Ordering::Relaxed); // approximate
        }
        Ok(())
    }

    /// Get a buffer by handle for internal use.
    fn get_buffer(&self, handle: &UnifiedBufferHandle) -> Option<&UnifiedBuffer> {
        self.heaps.get(&handle.heap_id)?.buffers.get(&handle.offset)
    }

    /// Get a mutable buffer by handle for internal use.
    fn get_buffer_mut(&mut self, handle: &UnifiedBufferHandle) -> Option<&mut UnifiedBuffer> {
        self.heaps.get_mut(&handle.heap_id)?.buffers.get_mut(&handle.offset)
    }
}

impl UnifiedHeap {
    /// Open this heap on the dGPU device using the shared NT handle.
    fn open_on_dgpu(&mut self, dgpu_device: &ID3D12Device) -> WinResult<()> {
        if self.dgpu_heap.is_some() {
            return Ok(());
        }
        let Some(ref safe_handle) = self.shared_handle else {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        };
        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            dgpu_device.OpenSharedHandle(safe_handle.get(), &mut heap)?;
        }
        self.dgpu_heap = heap;
        self.dgpu_device = Some(dgpu_device.clone());
        Ok(())
    }
}

/// Public API: initialize unified memory (called automatically on first use).
pub fn init() -> WinResult<()> {
    drop(UnifiedMemory::get_or_init());
    Ok(())
}

/// Public API: attach a dGPU device for cross-adapter unified memory.
pub fn attach_dgpu(device: ID3D12Device) -> WinResult<()> {
    UnifiedMemory::attach_dgpu(device)
}

/// Public API: create a unified heap of `size` bytes.
pub fn create_heap(size: u64) -> WinResult<UnifiedBufferHandle> {
    UnifiedMemory::create_heap(size)
}

/// Public API: allocate a buffer in a unified heap.
pub fn allocate_buffer(heap: &UnifiedBufferHandle, size: u64, flags: D3D12_RESOURCE_FLAGS) -> WinResult<UnifiedBufferHandle> {
    UnifiedMemory::allocate_buffer(heap, size, flags)
}

/// Public API: release a buffer.
pub fn release_buffer(handle: &UnifiedBufferHandle) -> WinResult<()> {
    UnifiedMemory::release_buffer(handle)
}

/// Public API: get CPU pointer for a buffer.
pub fn cpu_ptr(handle: &UnifiedBufferHandle) -> Option<*mut u8> {
    handle.cpu_ptr()
}

/// Public API: get APU GPU virtual address.
pub fn apu_gpu_va(handle: &UnifiedBufferHandle) -> Option<u64> {
    handle.apu_gpu_va()
}

/// Public API: get dGPU GPU virtual address.
pub fn dgpu_gpu_va(handle: &UnifiedBufferHandle) -> Option<u64> {
    handle.dgpu_gpu_va()
}

/// Public API: get buffer size.
pub fn buffer_size(handle: &UnifiedBufferHandle) -> Option<u64> {
    handle.size()
}

/// Public API: get buffer residency.
pub fn buffer_residency(handle: &UnifiedBufferHandle) -> Option<UnifiedResidency> {
    handle.residency()
}

/// Public API: migrate buffer to dGPU (caller must execute copy on dGPU queue).
pub fn migrate_to_dgpu(handle: &UnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.migrate_to_dgpu(list)
}

/// Public API: migrate buffer to APU (caller must execute copy on dGPU queue).
pub fn migrate_to_apu(handle: &UnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.migrate_to_apu(list)
}

/// Public API: finalize migration after copy completes.
pub fn finalize_migration(handle: &UnifiedBufferHandle, to_dgpu: bool) -> WinResult<()> {
    handle.finalize_migration(to_dgpu)
}

/// Print unified memory statistics.
pub fn print_stats() {
    let s: Vec<u64> = STATS.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    tracing::info!(
        "unified memory: {} heaps created, {} destroyed, {} buffers placed, {} released, {} MB current, {} APU→dGPU migrations, {} dGPU→APU migrations, {} coherency flushes",
        s[0], s[1], s[2], s[3], s[4] >> 20, s[5], s[6], s[7]
    );
}

// ============================================================================
// Xbox Unified Memory Emulation Public API
// ============================================================================

/// Initialize the Xbox unified memory emulation system.
/// This creates the GPU Optimal and Standard pools in VRAM.
pub fn xbox_unified_init() -> WinResult<()> {
    let config = xbox_config();
    tracing::info!(
        "Xbox unified memory: GPU Optimal pool = {} MB, Standard pool = {} MB, SFS = {}, BCPack = {}, GPU I/O = {}",
        config.gpu_optimal_vram_mb >> 20,
        config.standard_vram_mb >> 20,
        config.enable_sfs_emulation,
        config.enable_bcpack_decompression,
        config.enable_gpu_driven_io
    );
    Ok(())
}

/// Create a buffer in the Xbox emulated unified memory.
/// The buffer is placed in the appropriate pool based on target_pool.
pub fn xbox_unified_allocate_buffer(
    size: u64,
    flags: D3D12_RESOURCE_FLAGS,
    target_pool: XboxMemoryPool,
) -> WinResult<XboxUnifiedBufferHandle> {
    let config = xbox_config();
    let mut unified = XBOX_UNIFIED.get_or_init(|| {
        Mutex::new(XboxUnifiedMemory::new(config.clone()))
    }).lock().unwrap();

    let handle = unified.allocate_buffer(size, flags, target_pool)?;
    match target_pool {
        XboxMemoryPool::GpuOptimal => XBOX_STATS[0].fetch_add(1, Ordering::Relaxed),
        XboxMemoryPool::Standard => XBOX_STATS[1].fetch_add(1, Ordering::Relaxed),
        XboxMemoryPool::System => XBOX_STATS[2].fetch_add(1, Ordering::Relaxed),
    };
    Ok(handle)
}

/// Migrate buffer to GPU Optimal pool (VRAM, full bandwidth).
pub fn xbox_unified_migrate_to_gpu_optimal(handle: &XboxUnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.migrate_to_gpu_optimal(list)
}

/// Migrate buffer to Standard pool (VRAM or system RAM).
pub fn xbox_unified_migrate_to_standard(handle: &XboxUnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.migrate_to_standard(list)
}

/// Migrate buffer to System pool (system RAM).
pub fn xbox_unified_migrate_to_system(handle: &XboxUnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.migrate_to_system(list)
}

/// Finalize migration after copy completes.
pub fn xbox_unified_finalize_migration(handle: &XboxUnifiedBufferHandle, target_residency: Residency) -> WinResult<()> {
    handle.finalize_migration(target_residency)
}

/// Record texture mip access for SFS emulation.
pub fn xbox_unified_record_mip_access(handle: &XboxUnifiedBufferHandle, mip_level: u32) -> WinResult<()> {
    handle.record_mip_access(mip_level)
}

/// Process SFS feedback and load required mips (call per frame).
pub fn xbox_unified_process_sfs_feedback(handle: &XboxUnifiedBufferHandle, list: &HostList) -> WinResult<()> {
    handle.process_sfs_feedback(list)
}

/// Print Xbox unified memory statistics.
pub fn xbox_unified_print_stats() {
    let s: Vec<u64> = XBOX_STATS.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let config = xbox_config();
    tracing::info!(
        "Xbox unified memory: GPU Optimal allocs={} Standard allocs={} System allocs={} \
         GPU Optimal bytes={} MB Standard bytes={} MB System bytes={} MB \
         GpuOptimal→Standard={} Standard→GpuOptimal={} Standard→System={} System→Standard={} \
         SFS requests={} SFS mips loaded={} BCPack decompressions={} GPU I/O={} PCIe bytes={} MB Residency changes={}",
        s[0], s[1], s[2],
        s[3] >> 20, s[4] >> 20, s[5] >> 20,
        s[6], s[7], s[8], s[9],
        s[10], s[11], s[12], s[13], s[14] >> 20, s[15]
    );
    tracing::info!(
        "  Config: GPU Optimal VRAM={} MB, Standard VRAM={} MB, PCIe={} GB/s, VRAM={} GB/s",
        config.gpu_optimal_vram_mb >> 20,
        config.standard_vram_mb >> 20,
        config.pcie_bandwidth_gbps,
        config.vram_bandwidth_gbps
    );
}

/// Internal Xbox unified memory system state.
static XBOX_UNIFIED: OnceLock<Mutex<XboxUnifiedMemory>> = OnceLock::new();

/// Xbox unified memory system managing the three pools.
#[derive(Debug)]
struct XboxUnifiedMemory {
    config: XboxMemoryConfig,
    apu_device: ID3D12Device,
    dgpu_device: Option<ID3D12Device>,
    /// GPU Optimal pool: dGPU-local VRAM buffers
    gpu_optimal_heaps: Vec<ID3D12Heap>,
    gpu_optimal_bytes: AtomicU64,
    /// Standard pool: cross-adapter heaps (can be in VRAM or system RAM)
    standard_heaps: Vec<XboxStandardHeap>,
    standard_bytes: AtomicU64,
    /// System pool: system RAM via cross-adapter heaps
    system_heaps: Vec<UnifiedHeap>,
    system_bytes: AtomicU64,
    /// Buffers tracked by handle
    buffers: HashMap<u64, XboxUnifiedBuffer>,
    next_handle_id: u64,
    current_frame: u64,
}

/// A heap in the Standard pool — cross-adapter shared heap.
#[derive(Debug)]
struct XboxStandardHeap {
    heap: ID3D12Heap,
    dgpu_heap: Option<ID3D12Heap>,
    shared_handle: Option<SafeHandle>,
    size: u64,
    cursor: u64,
    in_vram: bool, // true = currently in VRAM, false = in system RAM
}

impl XboxUnifiedMemory {
    fn new(config: XboxMemoryConfig) -> Self {
        let host = crate::host::host().expect("host device not initialized");

        Self {
            config,
            apu_device: host.device.clone(),
            dgpu_device: None,
            gpu_optimal_heaps: Vec::new(),
            gpu_optimal_bytes: AtomicU64::new(0),
            standard_heaps: Vec::new(),
            standard_bytes: AtomicU64::new(0),
            system_heaps: Vec::new(),
            system_bytes: AtomicU64::new(0),
            buffers: HashMap::new(),
            next_handle_id: 1,
            current_frame: 0,
        }
    }

    fn allocate_buffer(
        &mut self,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        target_pool: XboxMemoryPool,
    ) -> WinResult<XboxUnifiedBufferHandle> {
        let aligned_size = size.next_multiple_of(64 * 1024);
        let handle_id = self.next_handle_id;
        self.next_handle_id += 1;

        let (apu_buffer, dgpu_buffer, cpu_mapping, apu_gpu_va, dgpu_gpu_va, residency) = match target_pool {
            XboxMemoryPool::GpuOptimal => {
                // Allocate in dGPU-local VRAM (GPU Optimal pool)
                self.ensure_gpu_optimal_heap(aligned_size)?;
                let heap = self.gpu_optimal_heaps.last().unwrap().clone();
                let offset = self.allocate_from_gpu_optimal_heap(aligned_size)?;

                let buffer_desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: aligned_size.max(1),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
                    ..Default::default()
                };

                let mut apu_buf: Option<ID3D12Resource> = None;
                unsafe {
                    self.apu_device.CreatePlacedResource(
                        &heap,
                        offset,
                        &buffer_desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut apu_buf,
                    )?;
                }
                let apu_buffer = apu_buf.ok_or_else(windows::core::Error::empty)?;

                let mut dgpu_buf: Option<ID3D12Resource> = None;
                if let Some(dgpu_device) = &self.dgpu_device {
                    unsafe {
                        dgpu_device.CreatePlacedResource(
                            &heap,
                            offset,
                            &buffer_desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut dgpu_buf,
                        )?;
                    }
                }
                let dgpu_buffer = dgpu_buf;

                let apu_gpu_va = unsafe { apu_buffer.GetGPUVirtualAddress() };
                let dgpu_gpu_va = dgpu_buffer.as_ref().map(|b| unsafe { b.GetGPUVirtualAddress() });

                // For GPU Optimal, we don't CPU map (it's in VRAM)
                let cpu_mapping = None;

                (apu_buffer, dgpu_buffer, cpu_mapping, apu_gpu_va, dgpu_gpu_va, Residency::GpuOptimal)
            }
            XboxMemoryPool::Standard => {
                // Allocate in cross-adapter heap (Standard pool)
                self.ensure_standard_heap(aligned_size)?;
                let heap_idx = self.standard_heaps.len() - 1;
                let heap = &mut self.standard_heaps[heap_idx];
                let offset = heap.cursor.next_multiple_of(64 * 1024);
                heap.cursor = offset + aligned_size;

                let buffer_desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: aligned_size.max(1),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
                    ..Default::default()
                };

                let mut apu_buf: Option<ID3D12Resource> = None;
                unsafe {
                    self.apu_device.CreatePlacedResource(
                        &heap.heap,
                        offset,
                        &buffer_desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut apu_buf,
                    )?;
                }
                let apu_buffer = apu_buf.ok_or_else(windows::core::Error::empty)?;

                let mut dgpu_buf: Option<ID3D12Resource> = None;
                if let Some(dgpu_device) = &self.dgpu_device
                    && let Some(dgpu_heap) = &heap.dgpu_heap {
                    unsafe {
                        dgpu_device.CreatePlacedResource(
                            dgpu_heap,
                            offset,
                            &buffer_desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut dgpu_buf,
                        )?;
                    }
                }
                let dgpu_buffer = dgpu_buf;

                let mut cpu_mapping: *mut c_void = std::ptr::null_mut();
                unsafe {
                    apu_buffer.Map(0, None, Some(&mut cpu_mapping))?;
                }
                let cpu_mapping_ptr = cpu_mapping.cast::<u8>();
                let cpu_mapping = CpuPtr(NonNull::new(cpu_mapping_ptr).unwrap());

                let apu_gpu_va = unsafe { apu_buffer.GetGPUVirtualAddress() };
                let dgpu_gpu_va = dgpu_buffer.as_ref().map(|b| unsafe { b.GetGPUVirtualAddress() });

                (apu_buffer, dgpu_buffer, Some(cpu_mapping), apu_gpu_va, dgpu_gpu_va, Residency::Standard)
            }
            XboxMemoryPool::System => {
                // Allocate in system RAM cross-adapter heap
                self.ensure_system_heap(aligned_size)?;
                let heap_idx = self.system_heaps.len() - 1;
                let heap = &mut self.system_heaps[heap_idx];
                let offset = heap.cursor.next_multiple_of(64 * 1024);
                heap.cursor = offset + aligned_size;

                let buffer_desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: aligned_size.max(1),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
                    ..Default::default()
                };

                let mut apu_buf: Option<ID3D12Resource> = None;
                unsafe {
                    self.apu_device.CreatePlacedResource(
                        &heap.heap,
                        offset,
                        &buffer_desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut apu_buf,
                    )?;
                }
                let apu_buffer = apu_buf.ok_or_else(windows::core::Error::empty)?;

                let mut dgpu_buf: Option<ID3D12Resource> = None;
                if let Some(dgpu_device) = &self.dgpu_device
                    && let Some(dgpu_heap) = &heap.dgpu_heap {
                    unsafe {
                        dgpu_device.CreatePlacedResource(
                            dgpu_heap,
                            offset,
                            &buffer_desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut dgpu_buf,
                        )?;
                    }
                }
                let dgpu_buffer = dgpu_buf;

                let mut cpu_mapping: *mut c_void = std::ptr::null_mut();
                unsafe {
                    apu_buffer.Map(0, None, Some(&mut cpu_mapping))?;
                }
                let cpu_mapping_ptr = cpu_mapping.cast::<u8>();
                let cpu_mapping = CpuPtr(NonNull::new(cpu_mapping_ptr).unwrap());

                let apu_gpu_va = unsafe { apu_buffer.GetGPUVirtualAddress() };
                let dgpu_gpu_va = dgpu_buffer.as_ref().map(|b| unsafe { b.GetGPUVirtualAddress() });

                (apu_buffer, dgpu_buffer, Some(cpu_mapping), apu_gpu_va, dgpu_gpu_va, Residency::System)
            }
        };

        // Create fences for synchronization
        let mut apu_to_dgpu_fence: Option<ID3D12Fence> = None;
        let mut dgpu_to_apu_fence: Option<ID3D12Fence> = None;
        if dgpu_buffer.is_some() {
            unsafe {
                let apu_fence: ID3D12Fence = self.apu_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
                apu_to_dgpu_fence = Some(apu_fence);
                if let Some(dgpu_device) = &self.dgpu_device {
                    let dgpu_fence: ID3D12Fence = dgpu_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
                    dgpu_to_apu_fence = Some(dgpu_fence);
                }
            }
        }

        let buffer = XboxUnifiedBuffer {
            apu_buffer,
            dgpu_buffer,
            offset: 0, // Not used in Xbox model
            size: aligned_size,
            cpu_mapping,
            apu_gpu_va,
            dgpu_gpu_va,
            residency,
            target_pool,
            apu_to_dgpu_fence,
            dgpu_to_apu_fence,
            sfs_feedback: None,
            access_frequency: 0,
            last_access_frame: self.current_frame,
        };

        self.buffers.insert(handle_id, buffer);

        // Update pool byte counters
        match target_pool {
            XboxMemoryPool::GpuOptimal => {
                self.gpu_optimal_bytes.fetch_add(aligned_size, Ordering::Relaxed);
            }
            XboxMemoryPool::Standard => {
                self.standard_bytes.fetch_add(aligned_size, Ordering::Relaxed);
            }
            XboxMemoryPool::System => {
                self.system_bytes.fetch_add(aligned_size, Ordering::Relaxed);
            }
        }

        Ok(XboxUnifiedBufferHandle { heap_id: handle_id, offset: 0 })
    }

    fn ensure_gpu_optimal_heap(&mut self, needed: u64) -> WinResult<()> {
        let total_needed = self.gpu_optimal_bytes.load(Ordering::Relaxed) + needed;
        let max_allowed = self.config.gpu_optimal_vram_mb * 1024 * 1024;

        if total_needed > max_allowed {
            self.evict_from_gpu_optimal(needed)?;
        }

        // Try to allocate from existing heaps
        for heap in &self.gpu_optimal_heaps {
            // Check if there's room (simplified - real impl would track free space)
        }

        // Create new heap if needed
        let heap_size = (self.config.gpu_optimal_vram_mb * 1024 * 1024).min(256 * 1024 * 1024);

        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            CreationNodeMask: 1,
            VisibleNodeMask: 0,
            ..Default::default()
        };

        let heap_flags = D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES;

        let heap_desc = D3D12_HEAP_DESC {
            SizeInBytes: heap_size,
            Properties: heap_props,
            Alignment: 64 * 1024,
            Flags: heap_flags,
        };

        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            if let Some(dgpu_device) = &self.dgpu_device {
                dgpu_device.CreateHeap(&heap_desc, &mut heap)?;
            } else {
                self.apu_device.CreateHeap(&heap_desc, &mut heap)?;
            }
        }
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        self.gpu_optimal_heaps.push(heap);
        Ok(())
    }

    fn allocate_from_gpu_optimal_heap(&mut self, size: u64) -> WinResult<u64> {
        // Simplified: just return 0 for the first allocation
        // Real implementation would track offsets in each heap
        Ok(0)
    }

    fn ensure_standard_heap(&mut self, needed: u64) -> WinResult<()> {
        let total_needed = self.standard_bytes.load(Ordering::Relaxed) + needed;
        let max_allowed = self.config.standard_vram_mb * 1024 * 1024;

        if total_needed > max_allowed {
            // Evict to system pool
            self.evict_from_standard(needed)?;
        }

        if self.standard_heaps.is_empty() || self.standard_heaps.last().unwrap().cursor + needed > self.standard_heaps.last().unwrap().size {
            let heap_size = (self.config.standard_vram_mb * 1024 * 1024).min(256 * 1024 * 1024).max(needed.next_multiple_of(64 * 1024));

            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                CreationNodeMask: 1,
                VisibleNodeMask: 0,
                ..Default::default()
            };

            let heap_flags = D3D12_HEAP_FLAG_SHARED
                | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER
                | D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES;

            let heap_desc = D3D12_HEAP_DESC {
                SizeInBytes: heap_size,
                Properties: heap_props,
                Alignment: 64 * 1024,
                Flags: heap_flags,
            };

            let mut heap: Option<ID3D12Heap> = None;
            unsafe {
                self.apu_device.CreateHeap(&heap_desc, &mut heap)?;
            }
            let heap = heap.ok_or_else(windows::core::Error::empty)?;

            // Create shared handle for dGPU
            let mut shared_handle: Option<SafeHandle> = None;
            if let Some(dgpu_device) = &self.dgpu_device {
                unsafe {
                    let handle = self.apu_device.CreateSharedHandle(
                        &heap,
                        None,
                        GENERIC_ALL,
                        None,
                    )?;
                    shared_handle = Some(SafeHandle(handle));
                }
            }

            // Open on dGPU
            let dgpu_heap = if let Some(dgpu_device) = &self.dgpu_device
                && let Some(ref safe_handle) = shared_handle {
                let mut dgpu_h: Option<ID3D12Heap> = None;
                unsafe {
                    dgpu_device.OpenSharedHandle(safe_handle.get(), &mut dgpu_h)?;
                }
                dgpu_h
            } else {
                None
            };

            self.standard_heaps.push(XboxStandardHeap {
                heap,
                dgpu_heap,
                shared_handle,
                size: heap_size,
                cursor: 0,
                in_vram: true,
            });
        }
        Ok(())
    }

    fn ensure_system_heap(&mut self, needed: u64) -> WinResult<()> {
        if self.system_heaps.is_empty() || self.system_heaps.last().unwrap().cursor + needed > self.system_heaps.last().unwrap().size {
            let heap_size = 256 * 1024 * 1024; // 256 MB system heaps

            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                CreationNodeMask: 1,
                VisibleNodeMask: 0,
                ..Default::default()
            };

            let heap_flags = D3D12_HEAP_FLAG_SHARED
                | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER
                | D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES;

            let heap_desc = D3D12_HEAP_DESC {
                SizeInBytes: heap_size,
                Properties: heap_props,
                Alignment: 64 * 1024,
                Flags: heap_flags,
            };

            let mut heap: Option<ID3D12Heap> = None;
            unsafe {
                self.apu_device.CreateHeap(&heap_desc, &mut heap)?;
            }
            let heap = heap.ok_or_else(windows::core::Error::empty)?;

            let mut shared_handle: Option<SafeHandle> = None;
            if let Some(dgpu_device) = &self.dgpu_device {
                unsafe {
                    let handle = self.apu_device.CreateSharedHandle(
                        &heap,
                        None,
                        GENERIC_ALL,
                        None,
                    )?;
                    shared_handle = Some(SafeHandle(handle));
                }
            }

            let dgpu_heap = if let Some(dgpu_device) = &self.dgpu_device
                && let Some(ref safe_handle) = shared_handle {
                let mut dgpu_h: Option<ID3D12Heap> = None;
                unsafe {
                    dgpu_device.OpenSharedHandle(safe_handle.get(), &mut dgpu_h)?;
                }
                dgpu_h
            } else {
                None
            };

            self.system_heaps.push(UnifiedHeap {
                heap,
                size: heap_size,
                dgpu_device: self.dgpu_device.clone(),
                dgpu_heap,
                shared_handle,
                buffers: HashMap::new(),
                cursor: 0,
            });
        }
        Ok(())
    }

    fn evict_from_gpu_optimal(&mut self, needed: u64) -> WinResult<()> {
        // Find a buffer in GPU Optimal pool to evict to Standard
        let mut victim_handle = None;
        for (handle_id, buffer) in &self.buffers {
            if buffer.residency == Residency::GpuOptimal {
                victim_handle = Some(*handle_id);
                break;
            }
        }

        if let Some(handle_id) = victim_handle {
            if let Some(buffer) = self.buffers.get_mut(&handle_id) {
                buffer.target_pool = XboxMemoryPool::Standard;
                buffer.residency = Residency::InFlight;
                self.gpu_optimal_bytes.fetch_sub(buffer.size, Ordering::Relaxed);
                self.standard_bytes.fetch_add(buffer.size, Ordering::Relaxed);
                XBOX_STATS[6].fetch_add(1, Ordering::Relaxed); // GPU Optimal → Standard
                XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
            }
        }
        Ok(())
    }

    fn evict_from_standard(&mut self, needed: u64) -> WinResult<()> {
        // Find a buffer in Standard pool to evict to System
        let mut victim_handle = None;
        for (handle_id, buffer) in &self.buffers {
            if buffer.residency == Residency::Standard {
                victim_handle = Some(*handle_id);
                break;
            }
        }

        if let Some(handle_id) = victim_handle {
            if let Some(buffer) = self.buffers.get_mut(&handle_id) {
                buffer.target_pool = XboxMemoryPool::System;
                buffer.residency = Residency::InFlight;
                self.standard_bytes.fetch_sub(buffer.size, Ordering::Relaxed);
                self.system_bytes.fetch_add(buffer.size, Ordering::Relaxed);
                XBOX_STATS[8].fetch_add(1, Ordering::Relaxed); // Standard → System
                XBOX_STATS[15].fetch_add(1, Ordering::Relaxed); // Residency changes
            }
        }
        Ok(())
    }

    /// Attach dGPU device for cross-adapter sharing.
    pub fn attach_dgpu(&mut self, device: ID3D12Device) -> WinResult<()> {
        let apu_luid = unsafe { self.apu_device.GetAdapterLuid() };
        let dgpu_luid = unsafe { device.GetAdapterLuid() };
        if apu_luid.LowPart == dgpu_luid.LowPart && apu_luid.HighPart == dgpu_luid.HighPart {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        self.dgpu_device = Some(device.clone());

        // Re-open standard heaps on dGPU
        for heap in &mut self.standard_heaps {
            if let Some(ref safe_handle) = heap.shared_handle {
                let mut dgpu_h: Option<ID3D12Heap> = None;
                unsafe {
                    device.OpenSharedHandle(safe_handle.get(), &mut dgpu_h)?;
                }
                heap.dgpu_heap = dgpu_h;
            }
        }

        // Re-open system heaps on dGPU
        for heap in &mut self.system_heaps {
            if let Some(ref safe_handle) = heap.shared_handle {
                let mut dgpu_h: Option<ID3D12Heap> = None;
                unsafe {
                    device.OpenSharedHandle(safe_handle.get(), &mut dgpu_h)?;
                }
                heap.dgpu_heap = dgpu_h;
            }
        }

        tracing::info!(
            apu_luid = format_args!("{:#x}:{:#x}", apu_luid.HighPart, apu_luid.LowPart),
            dgpu_luid = format_args!("{:#x}:{:#x}", dgpu_luid.HighPart, dgpu_luid.LowPart),
            "Xbox unified memory: attached dGPU device"
        );
        Ok(())
    }
}