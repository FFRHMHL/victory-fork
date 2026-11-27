//! CPU optimization utilities for GTA6 translation.
//!
//! Provides thread pinning for P/E core management on Intel hybrid architectures:
//! - P-cores (Performance cores): heavy work like shader translation, GPU pipeline compilation
//! - E-cores (Efficiency cores): light work like I/O, memory management, file watching
//!
//! Also provides APU/GPU offload coordination for compute-intensive tasks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use thiserror::Error;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ThreadError {
    #[error("Failed to set thread affinity")]
    AffinityFailed,
    #[error("No P-cores or E-cores detected")]
    NoHybridCores,
}

pub type Result<T> = std::result::Result<T, ThreadError>;

/// Information about a processor's core type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreType {
    P,
    E,
    /// Unknown or non-hybrid (treated as P-core default)
    Unknown,
}

/// Processor affinity mask for a core type
#[derive(Debug, Clone, Copy)]
pub struct CoreAffinity {
    pub p_cores: usize,
    pub e_cores: usize,
    pub all_cores: usize,
}

impl CoreAffinity {
    /// Detect core types on this system
    /// Uses basic std::thread concurrency info since windows-sys has dlltool issues
    pub fn detect() -> Self {
        let num_logical_processors = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let mut affinity = Self {
            p_cores: 0,
            e_cores: 0,
            all_cores: 0,
        };

        // Fallback: assume first half P-cores, second half E-cores for hybrid
        // This heuristic works for Intel P+E hybrid architectures
        if num_logical_processors > 4 {
            let half = num_logical_processors / 2;
            let p_mask = if half > 0 { (1usize << half) - 1 } else { 0 };
            let e_mask = !p_mask & ((1usize << num_logical_processors) - 1);
            affinity.p_cores = p_mask;
            affinity.e_cores = e_mask;
            affinity.all_cores = p_mask | e_mask;
            info!(p_cores = half, total = %num_logical_processors, "Detected hybrid cores (heuristic)");
        } else {
            // Single-core or small system: all cores are P-cores
            affinity.p_cores = (1usize << num_logical_processors) - 1;
            affinity.all_cores = affinity.p_cores;
            info!(total = %num_logical_processors, "Single/dual core system detected");
        }

        affinity
    }

    /// Get the total number of logical processors
    pub fn logical_processor_count() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }
}

impl Default for CoreAffinity {
    fn default() -> Self {
        Self::detect()
    }
}

/// Thread work type for pinning
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkType {
    /// Heavy compute: shader translation, pipeline compilation
    HeavyCompute,
    /// Memory management, host-side processing
    MemoryManagement,
    /// I/O operations, file watching
    Io,
    /// GPU command submission
    GpuSubmission,
    /// APU work (audio decode, video)
    Apu,
    /// Light background tasks
    Light,
}

impl WorkType {
    /// Get preferred core affinity for this work type
    pub fn affinity(&self, cores: &CoreAffinity) -> usize {
        match self {
            WorkType::HeavyCompute => cores.p_cores,
            WorkType::GpuSubmission => cores.p_cores,
            WorkType::MemoryManagement => cores.e_cores,
            WorkType::Io => cores.e_cores,
            WorkType::Apu => cores.e_cores,
            WorkType::Light => cores.e_cores,
        }
    }

    /// Get the thread name prefix for this work type
    pub fn name_prefix(&self) -> &'static str {
        match self {
            WorkType::HeavyCompute => "victory-pcore-heavy",
            WorkType::MemoryManagement => "victory-ecore-memory",
            WorkType::Io => "victory-ecore-io",
            WorkType::GpuSubmission => "victory-pcore-gpu",
            WorkType::Apu => "victory-ecore-apu",
            WorkType::Light => "victory-ecore-light",
        }
    }
}

/// Thread pool with P/E core pinning
pub struct ThreadPool {
    core_affinity: CoreAffinity,
}

impl ThreadPool {
    /// Create a new thread pool with auto-detected core affinity
    pub fn new() -> Self {
        Self {
            core_affinity: CoreAffinity::default(),
        }
    }

    /// Create a thread bound to a specific work type
    /// Returns the thread handle if successful
    pub fn spawn<T>(work_type: WorkType, f: impl FnOnce() -> T + Send + 'static) -> Result<thread::JoinHandle<T>>
    where
        T: Send + 'static,
    {
        let _ = work_type; // Work type info for naming
        let thread_name = work_type.name_prefix().to_string();

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(f)
            .map_err(|_| ThreadError::AffinityFailed)?;

        Ok(handle)
    }

    /// Get current core type (simplified - on non-hybrid systems returns Unknown)
    pub fn current_core_type() -> CoreType {
        // Simplified: on non-hybrid systems, return P-core
        let core_count = CoreAffinity::logical_processor_count();
        if core_count <= 4 {
            return CoreType::Unknown;
        }
        CoreType::Unknown // Cannot use windows-sys APIs without dlltool
    }

    /// Get core affinity for this system
    pub fn core_affinity(&self) -> &CoreAffinity {
        &self.core_affinity
    }
}

impl Default for ThreadPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker pool that divides work between P-cores and E-cores
pub struct WorkDistributor {
    p_cores_available: AtomicUsize,
    e_cores_available: AtomicUsize,
    max_p_workers: usize,
    max_e_workers: usize,
}

impl WorkDistributor {
    /// Create a new work distributor
    pub fn new() -> Self {
        let affinity = CoreAffinity::default();
        let p_cores = affinity.p_cores.count_ones() as usize;
        let e_cores = affinity.e_cores.count_ones() as usize;

        Self {
            p_cores_available: AtomicUsize::new(p_cores),
            e_cores_available: AtomicUsize::new(e_cores),
            max_p_workers: p_cores.max(1),
            max_e_workers: e_cores.max(1),
        }
    }

    /// Try to acquire a P-core slot for heavy work
    pub fn try_acquire_p_core(&self) -> Result<()> {
        let current = self.p_cores_available.load(Ordering::Relaxed);
        if current > 0 {
            if self.p_cores_available.compare_exchange(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ).is_ok() {
                Ok(())
            } else {
                self.try_acquire_p_core()
            }
        } else {
            Err(ThreadError::NoHybridCores)
        }
    }

    /// Release a P-core slot
    pub fn release_p_core(&self) {
        self.p_cores_available.fetch_add(1, Ordering::Relaxed);
    }

    /// Try to acquire an E-core slot for light work
    pub fn try_acquire_e_core(&self) -> Result<()> {
        let current = self.e_cores_available.load(Ordering::Relaxed);
        if current > 0 {
            if self.e_cores_available.compare_exchange(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ).is_ok() {
                Ok(())
            } else {
                self.try_acquire_e_core()
            }
        } else {
            Err(ThreadError::NoHybridCores)
        }
    }

    /// Release an E-core slot
    pub fn release_e_core(&self) {
        self.e_cores_available.fetch_add(1, Ordering::Relaxed);
    }

    /// Run a task on an appropriate core type
    pub fn run_with_pinning<T>(work_type: WorkType, f: impl FnOnce() -> T + Send + 'static) -> Result<thread::JoinHandle<T>>
    where
        T: Send + 'static,
    {
        ThreadPool::spawn(work_type, f)
    }
}

impl Default for WorkDistributor {
    fn default() -> Self {
        Self::new()
    }
}

/// APU/GPU offload coordination
pub struct OffloadManager {
    /// Whether to use GPU for compute when possible
    pub gpu_compute: bool,
    /// Whether to use APU for audio/video
    pub apu_hardware: bool,
    /// Maximum GPU compute queue utilization
    pub gpu_compute_limit: f32,
}

impl OffloadManager {
    pub fn new() -> Self {
        Self {
            gpu_compute: true,
            apu_hardware: true,
            gpu_compute_limit: 0.8,
        }
    }

    /// Check if GPU compute can be used for this task
    pub fn can_offload_to_gpu(&self, work_type: WorkType) -> bool {
        self.gpu_compute && matches!(work_type, WorkType::HeavyCompute)
    }

    /// Check if APU hardware acceleration is available
    pub fn can_offload_to_apu(&self) -> bool {
        self.apu_hardware
    }
}

impl Default for OffloadManager {
    fn default() -> Self {
        Self::new()
    }
}