//! Rewind history: a ring of snapshots under a byte budget, newest last.

use std::collections::VecDeque;

/// Snapshot every this many emulated frames.
pub const EVERY: u32 = 2;
/// Seconds of history kept at most.
pub const SECONDS: usize = 20;
/// Most snapshots a ring holds: 20 s of 60 fps NTSC frames, one snapshot every 2 frames (600).
pub const CAPACITY: usize = SECONDS * 60 / EVERY as usize;
/// Memory a ring's snapshots may take, in bytes: 64 MiB. A NES snapshot is 10-20 KB (the largest
/// boards, with 64 KB of cartridge RAM, stay under 100 KB), so `CAPACITY` of them take 6-12 MB
/// and at most 60 MB: the NES always keeps its full `SECONDS`. A Mega Drive snapshot is about
/// 200 KB, so its ring holds about 330 snapshots (11 s).
pub const BUDGET_BYTES: usize = 64 << 20;

/// Ring of the newest items whose total size fits a byte budget, never more than `CAPACITY` and
/// never fewer than one. A push drops the oldest items while the ring is over either limit.
///
/// Every item of a ring counts as the same size, measured once by the caller (snapshots have no
/// size accessor): for snapshots, `console.encode_state(&console.snapshot()).len()`, the
/// serialized state, which is within a few percent of the snapshot's memory.
pub struct Ring<T> {
    items: VecDeque<T>,
    /// Size one item counts as, in bytes (at least 1).
    item_bytes: usize,
    budget_bytes: usize,
}

impl<T> Ring<T> {
    /// A ring that keeps `budget_bytes` of items counted as `snapshot_bytes` each.
    pub fn new(budget_bytes: usize, snapshot_bytes: usize) -> Ring<T> {
        Ring { items: VecDeque::new(), item_bytes: snapshot_bytes.max(1), budget_bytes }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn push(&mut self, item: T) {
        self.items.push_back(item);
        while self.items.len() > 1 && (self.items.len() > CAPACITY || self.items.len() * self.item_bytes > self.budget_bytes) {
            self.items.pop_front();
        }
    }

    /// Removes and returns the newest item (one step back in time).
    pub fn step_back(&mut self) -> Option<T> {
        self.items.pop_back()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_rewind_capacity() {
        assert_eq!(CAPACITY, 600);
        let mut r = Ring::new(3, 1);
        for i in 0..3 {
            r.push(i);
        }
        assert_eq!(r.len(), 3);
        r.push(3);
        assert_eq!(r.len(), 3);
    }

    /// The limit is bytes: big snapshots get a shorter history, small ones stop at `CAPACITY`,
    /// and one snapshot is kept even when it alone is over the budget.
    #[test]
    fn app_rewind_byte_budget() {
        let fill = |budget: usize, each: usize| {
            let mut r = Ring::new(budget, each);
            for i in 0..2 * CAPACITY {
                r.push(i);
            }
            assert_eq!(r.step_back(), Some(2 * CAPACITY - 1), "the newest survives");
            r.len() + 1
        };
        assert_eq!(fill(BUDGET_BYTES, 20_000), CAPACITY, "NES: the whole 20 s");
        assert_eq!(fill(BUDGET_BYTES, 110_000), CAPACITY, "the largest NES boards too");
        assert_eq!(fill(BUDGET_BYTES, 200_000), BUDGET_BYTES / 200_000, "Mega Drive: what fits");
        assert_eq!(fill(1000, 300), 3);
        assert_eq!(fill(1000, 5000), 1);
        assert_eq!(fill(1000, 0), CAPACITY, "an unmeasured size counts as one byte");
    }

    #[test]
    fn app_rewind_step_back_order() {
        let mut r = Ring::new(4, 1);
        assert_eq!(r.step_back(), None::<i32>);
        r.push(1);
        r.push(2);
        assert_eq!(r.len(), 2);
        assert_eq!(r.step_back(), Some(2));
        assert_eq!(r.step_back(), Some(1));
        assert_eq!(r.step_back(), None);
        assert!(r.is_empty());
    }

    #[test]
    fn app_rewind_wrap_drops_oldest() {
        let mut r = Ring::new(3, 1);
        for i in 0..10 {
            r.push(i);
        }
        let mut got = Vec::new();
        while let Some(x) = r.step_back() {
            got.push(x);
        }
        assert_eq!(got, [9, 8, 7]);
        // Push after stepping back continues from there.
        r.push(100);
        r.push(101);
        assert_eq!(r.step_back(), Some(101));
        r.clear();
        assert_eq!(r.len(), 0);
    }

    /// Counts heap bytes allocated by the current thread while `COUNTING` is on.
    struct Counting;

    thread_local! {
        static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if COUNTING.with(|c| c.get()) {
                BYTES.with(|b| b.set(b.get() + layout.size()));
            }
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOC: Counting = Counting;

    fn roms(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                roms(&p, out);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nes")) {
                out.push(p);
            }
        }
    }

    /// For every NES test ROM the ring, built as the app builds it, keeps the whole `CAPACITY`
    /// (the byte budget never shortens the NES's history), and a full ring of real snapshots
    /// (ring storage + snapshot heap) stays under 64 MiB.
    #[test]
    fn app_rewind_memory_budget() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ebobo-core/tests/roms");
        if !dir.is_dir() {
            println!("test ROMs missing: run scripts/fetch-test-roms.sh");
            return;
        }
        let mut files = Vec::new();
        roms(&dir, &mut files);
        assert!(!files.is_empty(), "no test ROMs in {}", dir.display());
        let mut worst = (0usize, std::path::PathBuf::new());
        let mut largest = 0usize;
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(mut console) = ebobo_console::Console::new(&bytes, None) else { continue };
            console.run_frame([0; 4]);
            let snapshot_bytes = console.encode_state(&console.snapshot()).len();
            largest = largest.max(snapshot_bytes);
            BYTES.with(|b| b.set(0));
            COUNTING.with(|c| c.set(true));
            let mut ring = Ring::new(BUDGET_BYTES, snapshot_bytes);
            for _ in 0..CAPACITY {
                ring.push(console.snapshot());
            }
            COUNTING.with(|c| c.set(false));
            assert_eq!(ring.len(), CAPACITY, "{} bytes a snapshot for {}", snapshot_bytes, path.display());
            let total = BYTES.with(|b| b.get());
            if total > worst.0 {
                worst = (total, path);
            }
        }
        eprintln!("rewind ring: worst {} KiB for {}; largest snapshot {} bytes", worst.0 / 1024, worst.1.display(), largest);
        assert!(worst.0 < 64 << 20, "{} bytes for {}", worst.0, worst.1.display());
    }
}
