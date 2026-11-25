//! Diagnostics (`FLAMINGO_WC_STAT=1`): which title memory the copy-back
//! writes into, by what the title said of that memory when it allocated
//! it. The console's `XMemVirtualAlloc` protection carries write-combining
//! (0x400), uncached (0x200) and GPU access bits (0x800 and up), which the
//! runtime masks off before Windows sees them (`xmem::HOST_PROTECTION`);
//! the title's own `VirtualAlloc` calls reach Windows as they are, so
//! write-combining there shows in `VirtualQuery`. Every ten seconds a line
//! gives the copy-back bytes by kind, and the title's allocations by
//! protection, to say whether copy-back is mostly memory the title itself
//! treats as written by the CPU and read by the GPU only.
//!
//! Measurement only: nothing here changes what is copied.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory as mem;

/// The console's write-combining and uncached protection bits.
const WC: u32 = 0x400;
const NOCACHE: u32 = 0x200;

const KINDS: [&str; 7] = [
    "XMemVirtualAlloc, write-combined",
    "XMemVirtualAlloc, uncached",
    "XMemVirtualAlloc, cached",
    "the title's VirtualAlloc, write-combined (host protection)",
    "XMemAlloc heap",
    "other private memory",
    "view memory or unknown",
];

/// Copy-back bytes and calls by kind since the start.
static BACK: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
static CALLS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// What the title said when it allocated `[start, end)`.
#[derive(Clone, Copy)]
struct Alloc {
    end: u64,
    protect: u32,
    flags: u64,
    heap: bool,
}

#[derive(Default)]
struct State {
    allocs: BTreeMap<u64, Alloc>,
    /// Kind found for a copy's first byte (copies keep their address).
    kinds: HashMap<u64, u8>,
    /// Allocations by (protection, from the heap): count, bytes.
    by_protect: BTreeMap<(u32, bool), (u64, u64)>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("FLAMINGO_WC_STAT").is_some_and(|v| v == "1");
        if on {
            start_logging();
        }
        on
    })
}

/// The runtime tells of a title allocation: `protect` and `flags` as the
/// title gave them (before any masking); `heap` for `XMemAlloc`'s.
pub fn title_memory_attributes(at: usize, size: usize, protect: u32, flags: u64, heap: bool) {
    if !on() || at == 0 || size == 0 {
        return;
    }
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    let start = at as u64;
    let end = start + size as u64;
    // A commit inside a reservation already known keeps the reservation's
    // attributes (one at the reservation's own start is taken as anew).
    if s.allocs.range(..start).next_back().is_some_and(|(_, a)| end <= a.end) {
        return;
    }
    s.allocs.insert(start, Alloc { end, protect, flags, heap });
    // Kinds found before for memory this allocation now covers are stale.
    s.kinds.retain(|&k, _| k < start || k >= end);
    let e = s.by_protect.entry((protect, heap)).or_default();
    e.0 += 1;
    e.1 += size as u64;
}

fn kind_of(s: &State, at: u64) -> u8 {
    if let Some((_, a)) = s.allocs.range(..=at).next_back().filter(|(_, a)| at < a.end) {
        if a.heap {
            return 4;
        }
        return if a.protect & WC != 0 {
            0
        } else if a.protect & NOCACHE != 0 {
            1
        } else {
            2
        };
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 6;
    }
    if info.Protect & mem::PAGE_WRITECOMBINE != 0 || info.AllocationProtect & mem::PAGE_WRITECOMBINE != 0 {
        return 3;
    }
    if info.Type == mem::MEM_PRIVATE { 5 } else { 6 }
}

/// `bytes` were written back into title memory by a copy whose first byte
/// is `start`.
pub(crate) fn back(start: u64, bytes: usize) {
    if bytes == 0 || !on() {
        return;
    }
    let kind = {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        match s.kinds.get(&start) {
            Some(&k) => k,
            None => {
                let k = kind_of(&s, start);
                s.kinds.insert(start, k);
                k
            }
        }
    } as usize;
    BACK[kind].fetch_add(bytes as u64, Relaxed);
    CALLS[kind].fetch_add(1, Relaxed);
}

fn start_logging() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        let mut last = [0u64; 7];
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let now: Vec<u64> = BACK.iter().map(|b| b.load(Relaxed)).collect();
            let all: u64 = now.iter().sum();
            let recent: u64 = now.iter().zip(&last).map(|(n, l)| n - l).sum();
            let mut line = format!(
                "wc stat: copy-back since the start {:.1} MB, last 10 s {:.1} MB, by what the title said of the memory:",
                all as f64 / 1e6,
                recent as f64 / 1e6
            );
            for (i, name) in KINDS.iter().enumerate() {
                if now[i] > 0 {
                    line += &format!(
                        " [{name}: {:.1} MB ({:.0}%), last 10 s {:.1} MB, {} merges]",
                        now[i] as f64 / 1e6,
                        100.0 * now[i] as f64 / all.max(1) as f64,
                        (now[i] - last[i]) as f64 / 1e6,
                        CALLS[i].load(Relaxed)
                    );
                }
            }
            tracing::info!("{line}");
            last.copy_from_slice(&now);
            let s = state().lock().unwrap_or_else(|e| e.into_inner());
            let mut rows: Vec<_> = s.by_protect.iter().map(|(&(p, h), &(n, b))| (b, n, p, h)).collect();
            rows.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let top: Vec<String> = rows
                .iter()
                .take(10)
                .map(|&(b, n, p, h)| format!("{}{p:#x}: {n} of {} MB", if h { "heap attributes " } else { "protect " }, b >> 20))
                .collect();
            tracing::info!("wc stat: title allocations by protection (largest first): {}", top.join("; "));
            let flags: Vec<String> = s
                .allocs
                .values()
                .filter(|a| a.protect & WC != 0 && !a.heap)
                .take(3)
                .map(|a| format!("flags {:#x}", a.flags))
                .collect();
            if !flags.is_empty() {
                tracing::info!("wc stat: write-combined allocations, first flags: {}", flags.join(", "));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_what_the_title_said() {
        let mut s = State::default();
        s.allocs.insert(0x1000_0000, Alloc { end: 0x1001_0000, protect: 0x404, flags: 0, heap: false });
        s.allocs.insert(0x2000_0000, Alloc { end: 0x2001_0000, protect: 0x204, flags: 0, heap: false });
        s.allocs.insert(0x3000_0000, Alloc { end: 0x3001_0000, protect: 0x4, flags: 0, heap: false });
        s.allocs.insert(0x4000_0000, Alloc { end: 0x4001_0000, protect: 0, flags: 0, heap: true });
        assert_eq!(kind_of(&s, 0x1000_8000), 0);
        assert_eq!(kind_of(&s, 0x2000_0000), 1);
        assert_eq!(kind_of(&s, 0x3000_fff0), 2);
        assert_eq!(kind_of(&s, 0x4000_0010), 4);
    }
}
