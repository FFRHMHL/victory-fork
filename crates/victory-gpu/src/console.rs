//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
    /// Custom profile: 1080p render target, RT on GPU, reduced memory for 12GB VRAM
    Custom1080pRT,
    /// Custom profile: 720p render target, software RT, minimal memory for lower VRAM
    Custom720p,
}

impl Console {
    /// Title memory in bytes: (GPU-optimal, standard). Series X in the
    /// advanced configuration; Series S has one range, counted as standard
    /// (7,776 MiB, the published console-mode figure).
    // shortcut: later system updates gave Series S titles more (amount not
    // published here); the title sizes nothing from this figure
    // (it goes by the device type), so update it if a title does.
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
            // Reduced pools for RTX 3500 Ada 12GB: 6GB GpuOptimal + 2GB Standard = 8GB
            // Leaves headroom for OS, driver, RT structures, and shader cache
            Console::Custom1080pRT => (6 * GIB, 2 * GIB),
            // Minimal pools for 720p: 4GB GpuOptimal + 1GB Standard = 5GB
            Console::Custom720p => (4 * GIB, 1 * GIB),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
            Console::Custom1080pRT => "Custom 1080p RT",
            Console::Custom720p => "Custom 720p",
        }
    }

    /// Device type reported to the title (1=Lockhart/SeriesS, 2=Anaconda/SeriesX)
    pub const fn device_type(self) -> u32 {
        match self {
            Console::SeriesX => 2,
            Console::SeriesS => 1,
            Console::Custom1080pRT => 2,
            Console::Custom720p => 2,
        }
    }

    /// Whether to force 1080p render target
    pub const fn force_1080p(self) -> bool {
        matches!(self, Console::Custom1080pRT)
    }

    /// Whether to force 720p render target
    pub const fn force_720p(self) -> bool {
        matches!(self, Console::Custom720p)
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

/// Set before the title starts.
pub fn set(console: Console) {
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}
