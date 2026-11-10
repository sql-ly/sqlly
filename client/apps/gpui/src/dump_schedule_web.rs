//! Browser (wasm) variant of [`crate::dump_schedule`]. Scheduling math and
//! types are identical; persistence is inert because the browser preview has
//! no filesystem-backed scheduled dumps.

include!("dump_schedule_impl.rs");

/// The store never changes in the browser (saves are inert), so its change
/// generation is constant.
pub fn store_generation() -> u64 {
    0
}

impl DumpSchedules {
    pub fn load() -> Self {
        Self::default()
    }

    pub fn load_checked() -> Result<Self, String> {
        Ok(Self::default())
    }

    pub fn save(&self) -> Result<(), String> {
        Ok(())
    }
}
