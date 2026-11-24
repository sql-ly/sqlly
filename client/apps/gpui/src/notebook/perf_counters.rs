//! Test-only counters for the large-notebook performance probes: how often
//! a frame or a keystroke built a cell, a header control, or walked the
//! whole notebook. Counts don't depend on machine load, so the probes and
//! regression tests assert on them.

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// A cell's subtree built by `render_cell`.
    CellBuilt,
    /// A button built through the notebook's button helpers.
    Button,
    /// The reference entries for IntelliSense rebuilt over every cell.
    References,
    /// Which cells folded headings hide, recomputed over every cell.
    Folds,
    /// Find and replace searched the whole notebook.
    Find,
    /// The left rail's search read every cell.
    Search,
    /// The outline rail's rows rebuilt over every cell.
    Outline,
    /// Staleness re-checked over every cell.
    Staleness,
    /// The undo history compared the whole document.
    History,
    /// The in-app trust recomputed over every cell.
    Trust,
}

const PASSES: usize = 10;

thread_local! {
    static COUNTS: [Cell<usize>; PASSES] = const { [const { Cell::new(0) }; PASSES] };
}

pub fn bump(pass: Pass) {
    COUNTS.with(|c| c[pass as usize].set(c[pass as usize].get() + 1));
}

pub fn get(pass: Pass) -> usize {
    COUNTS.with(|c| c[pass as usize].get())
}

/// Every counter as it stands, for a before/after difference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot([usize; PASSES]);

impl Snapshot {
    pub fn now() -> Self {
        Self(COUNTS.with(|c| std::array::from_fn(|i| c[i].get())))
    }

    pub fn since(self, before: Snapshot) -> Snapshot {
        Self(std::array::from_fn(|i| self.0[i] - before.0[i]))
    }

    pub fn of(&self, pass: Pass) -> usize {
        self.0[pass as usize]
    }

    /// Passes that walk every cell of the notebook.
    pub fn whole_notebook(&self) -> usize {
        [
            Pass::References,
            Pass::Folds,
            Pass::Find,
            Pass::Search,
            Pass::Outline,
            Pass::Staleness,
            Pass::History,
            Pass::Trust,
        ]
        .iter()
        .map(|p| self.of(*p))
        .sum()
    }
}

/// Counts heap allocations and live bytes for the whole test binary, so the
/// probes can report allocations per keystroke and bytes per cell.
struct CountingAlloc;

static ALLOCATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LIVE_BYTES: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        use std::sync::atomic::Ordering::Relaxed;
        ALLOCATIONS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(layout.size() as isize, Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as isize, std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        use std::sync::atomic::Ordering::Relaxed;
        ALLOCATIONS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(new_size as isize - layout.size() as isize, Relaxed);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: CountingAlloc = CountingAlloc;

pub fn allocations() -> usize {
    ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn live_bytes() -> isize {
    LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The one-minute load average, so a probe can say whether its timings
/// are trustworthy.
pub fn load_average() -> f64 {
    #[cfg(unix)]
    {
        let mut loads = [0f64; 3];
        if unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } > 0 {
            return loads[0];
        }
    }
    f64::NAN
}
