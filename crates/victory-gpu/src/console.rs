//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
    /// Custom profile: 1080p render target, RT on GPU, Series X memory
    Custom1080pRT,
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
            Console::Custom1080pRT => (10 * GIB, 7 * GIB / 2),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
            Console::Custom1080pRT => "Custom 1080p RT",
        }
    }

    /// Device type reported to the title (1=Lockhart/SeriesS, 2=Anaconda/SeriesX)
    pub const fn device_type(self) -> u32 {
        match self {
            Console::SeriesX => 2,
            Console::SeriesS => 1,
            Console::Custom1080pRT => 2,
        }
    }

    /// Whether to force 1080p render target
    pub const fn force_1080p(self) -> bool {
        matches!(self, Console::Custom1080pRT)
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
