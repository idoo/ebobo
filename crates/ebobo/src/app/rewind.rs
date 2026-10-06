//! Rewind history: a fixed-capacity ring of snapshots, newest last.

use std::collections::VecDeque;

/// Snapshot every this many emulated frames.
pub const EVERY: u32 = 2;
/// Seconds of history kept.
pub const SECONDS: usize = 20;
/// Ring capacity: 20 s of 60 fps NTSC frames, one snapshot every 2 frames (600).
pub const CAPACITY: usize = SECONDS * 60 / EVERY as usize;

/// Ring of the last `cap` items. When full, a push overwrites the oldest one in place (the
/// evicted item's allocation is reused by `push_with`).
pub struct Ring<T> {
    items: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    pub fn new(cap: usize) -> Ring<T> {
        assert!(cap > 0);
        Ring { items: VecDeque::with_capacity(cap), cap }
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
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back(item);
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
        let mut r = Ring::new(3);
        for i in 0..3 {
            r.push(i);
        }
        assert_eq!(r.len(), 3);
        r.push(3);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn app_rewind_step_back_order() {
        let mut r = Ring::new(4);
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
        let mut r = Ring::new(3);
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

    /// A full ring of real snapshots (ring storage + snapshot heap) stays under 64 MiB for
    /// every test ROM.
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
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(mut nes) = ebobo_core::Nes::new(&bytes, None) else { continue };
            nes.run_frame([0; 4]);
            BYTES.with(|b| b.set(0));
            COUNTING.with(|c| c.set(true));
            let mut ring = Ring::new(CAPACITY);
            for _ in 0..CAPACITY {
                ring.push(nes.snapshot());
            }
            COUNTING.with(|c| c.set(false));
            let total = BYTES.with(|b| b.get());
            if total > worst.0 {
                worst = (total, path);
            }
        }
        eprintln!("rewind ring: worst {} KiB for {}", worst.0 / 1024, worst.1.display());
        assert!(worst.0 < 64 << 20, "{} bytes for {}", worst.0, worst.1.display());
    }
}
