//! Diagnostics (`FLAMINGO_WC_TOUCH=1`): how often the title touches, on
//! the CPU, the write-combined memory that shaders wrote and the copy-back
//! put into title memory. The question it answers: could that copy-back be
//! left until the CPU's first touch (a fetch behind a no-access guard)
//! instead of being made after every submission?
//!
//! After a copy-back into memory the title allocated write-combined
//! ([`crate::wcstat`] kind 0), the 64 KiB pages it wrote are made
//! no-access. The first touch faults into [`on_touch`], which gives the
//! page back the protection it had and counts the touch:
//! - by the title (the faulting code, or the code a leaf copy routine
//!   returns to, lies outside our image): a read or a write, and the page
//!   stays open until the next copy-back writes it;
//! - by us (our uploads, compares and merges read the same memory): the
//!   page is armed again at the next copy-back, so our own work does not
//!   hide the title's touches for long. The counts are therefore a lower
//!   bound on the title's touches.
//!
//! Private memory and physical-page views are both armed, each page with
//! the protection it had remembered. A view page may be read-only: the
//! write watch's guard (`crate::watch`). It gets that back, so a write
//! faults again and the watch's handler (after ours in the line) takes it
//! as before. A page whose protection someone else changed meanwhile (the
//! watch guarding it, a remap) is no longer ours: the handler forgets it
//! and passes the fault on. A touch through another view of the same
//! physical page is not seen (a lower bound again).
//!
//! `FLAMINGO_WC_TOUCH_SAMPLE=n` (4) arms one page in `n` by address, so the
//! faults of our own touches leave the title running.
//!
//! A system call that reads or writes an armed page fails instead of
//! faulting; none of ours does (our copies try a plain copy first and this
//! handler runs ahead of theirs), a title's might.
//!
//! Measurement only: no copy-back is skipped, every byte stays right.

use std::collections::{BTreeMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory as mem;

const PAGE: usize = 64 << 10;

/// What an armed page had: its protection, and whether it is a view.
#[derive(Clone, Copy)]
struct Was {
    protect: u32,
    view: bool,
}

/// Pages made no-access and not touched since.
static ARMED: Mutex<BTreeMap<usize, Was>> = Mutex::new(BTreeMap::new());
/// Pages our own code touched: armed again at the next copy-back.
static REARM: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());
/// Pages the title touched in the current ten seconds: private, views.
static TITLE_PAGES: Mutex<[Option<HashSet<usize>>; 2]> = Mutex::new([None, None]);

/// Counters since the last line; `[private, view]` pairs.
static N: [[AtomicU64; 2]; 7] = [const { [const { AtomicU64::new(0) }; 2] }; 7];
const ARMS: usize = 0;
const TITLE_READS: usize = 1;
const TITLE_WRITES: usize = 2;
const OURS: usize = 3;
const REARMS: usize = 4;
/// Pages of other protections or kinds, not armed.
const SKIPPED: usize = 5;
/// Armed pages someone else changed before their first touch.
const LOST: usize = 6;

/// Our image: `[start, end)`.
static IMAGE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FLAMINGO_WC_TOUCH").is_some_and(|v| v == "1"))
}

fn sample() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("FLAMINGO_WC_TOUCH_SAMPLE").ok().and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(4)
    })
}

/// Whether a page of this protection may be armed and given it back.
fn armable(protect: u32) -> bool {
    protect == mem::PAGE_READWRITE || protect == mem::PAGE_READONLY
}

/// Makes `page` no-access; `armed` is held. Returns whether it is armed.
fn arm_page(armed: &mut BTreeMap<usize, Was>, page: usize, view: bool) -> bool {
    let mut old = 0;
    // SAFETY: committed title memory of a protection [`armable`] takes
    // (checked by the caller, or put back below); the handler's lock is
    // held.
    if unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_NOACCESS, &mut old) } == 0 {
        return false;
    }
    if !armable(old) {
        // SAFETY: as above: what it was.
        unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, old, &mut old) };
        return false;
    }
    armed.insert(page, Was { protect: old, view });
    true
}

/// Whether `page` is in the table and still no-access. One that someone
/// else opened meanwhile leaves the table: the write watch opens a view's
/// block before the copy-back writes it (`watch::writing`), without a
/// fault, so a page armed after the last copy-back is open again by the
/// next one, and must be armed again then.
fn still_armed(armed: &mut BTreeMap<usize, Was>, page: usize) -> bool {
    let Some(was) = armed.get(&page).copied() else {
        return false;
    };
    // SAFETY: plain data filled in by the query.
    let mut q: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery((page * PAGE) as *const c_void, &mut q, size_of_val(&q)) };
    if n != 0 && q.Protect == mem::PAGE_NOACCESS {
        return true;
    }
    armed.remove(&page);
    N[LOST][was.view as usize].fetch_add(1, Relaxed);
    false
}

/// The copy-back just wrote title memory `[from, to)`, write-combined.
pub(crate) fn arm(from: usize, to: usize) {
    if !on() || to <= from {
        return;
    }
    install();
    let rearm = std::mem::take(&mut *REARM.lock().unwrap_or_else(|e| e.into_inner()));
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    for (page, view) in rearm {
        if !still_armed(&mut armed, page) && arm_page(&mut armed, page, view) {
            N[REARMS][view as usize].fetch_add(1, Relaxed);
        }
    }
    let every = sample();
    let (first, last) = (from / PAGE, (to - 1) / PAGE);
    let mut page = first;
    while page <= last {
        if page % every != 0 || still_armed(&mut armed, page) {
            page += 1;
            continue;
        }
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { mem::VirtualQuery((page * PAGE) as *const c_void, &mut info, size_of_val(&info)) };
        let view = info.Type == mem::MEM_MAPPED;
        if n == 0 || info.State != mem::MEM_COMMIT || !(view || info.Type == mem::MEM_PRIVATE) || !armable(info.Protect) {
            N[SKIPPED][view as usize].fetch_add(1, Relaxed);
            page += 1;
            continue;
        }
        if arm_page(&mut armed, page, view) {
            N[ARMS][view as usize].fetch_add(1, Relaxed);
        }
        page += 1;
    }
}

fn ours(rip: usize, rsp: usize) -> bool {
    let (start, end) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
    if (start..end).contains(&rip) {
        return true;
    }
    // A leaf copy routine (the C runtime's memcpy) has its caller's return
    // address on top of the stack.
    // SAFETY: the faulting thread's stack pointer, at its top: readable.
    let ret = unsafe { (rsp as *const usize).read_unaligned() };
    (start..end).contains(&ret)
}

unsafe extern "system" fn on_touch(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH};
    // SAFETY: the system passes the exception being dispatched and the
    // context it will continue with.
    let (r, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION || r.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let (access, at) = (r.ExceptionInformation[0], r.ExceptionInformation[1]);
    let page = at / PAGE;
    let was = {
        let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(was) = armed.remove(&page) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        // Still no-access? Someone else may have changed it since we armed
        // it (the watch's guard, a remap): then the fault is not ours.
        // SAFETY: plain data filled in by the query.
        let mut q: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { mem::VirtualQuery((page * PAGE) as *const c_void, &mut q, size_of_val(&q)) };
        if n == 0 || q.Protect != mem::PAGE_NOACCESS {
            N[LOST][was.view as usize].fetch_add(1, Relaxed);
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let mut old = 0;
        // SAFETY: a page we made no-access; `was.protect` is what it had.
        unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, was.protect, &mut old) };
        was
    };
    let v = was.view as usize;
    if ours(context.Rip as usize, context.Rsp as usize) {
        N[OURS][v].fetch_add(1, Relaxed);
        REARM.lock().unwrap_or_else(|e| e.into_inner()).push((page, was.view));
    } else {
        N[if access == 1 { TITLE_WRITES } else { TITLE_READS }][v].fetch_add(1, Relaxed);
        TITLE_PAGES.lock().unwrap_or_else(|e| e.into_inner())[v].get_or_insert_with(HashSet::new).insert(page);
    }
    // A write to a page given back read-only faults again, into the
    // watch's handler: its guard works as before.
    EXCEPTION_CONTINUE_EXECUTION
}

/// Our handler, ahead of the write watch's and the plain copies' (each
/// added at the front of the line: theirs first, then ours), so that a
/// fault on an armed page is ours to open before they take a read fault
/// inside their copy for an unmapped page.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::watch::install();
        crate::plaincopy::install();
        // SAFETY: our own image's headers, mapped for the process's life.
        unsafe {
            let base = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize;
            let nt = base + (*((base + 0x3c) as *const u32)) as usize;
            let size = *((nt + 0x50) as *const u32) as usize;
            IMAGE[0].store(base, Relaxed);
            IMAGE[1].store(base + size, Relaxed);
            windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(1, Some(on_touch));
        }
        tracing::info!(
            "wc touch: armed (one page in {}); write-combined pages the copy-back writes are made no-access until their first touch",
            sample()
        );
    });
}

/// The ten-second line ([`crate::wcstat`]'s logging thread).
pub(crate) fn report() {
    if !on() {
        return;
    }
    let n: Vec<[u64; 2]> = N.iter().map(|c| [c[0].swap(0, Relaxed), c[1].swap(0, Relaxed)]).collect();
    let pages = std::mem::take(&mut *TITLE_PAGES.lock().unwrap_or_else(|e| e.into_inner())).map(|s| s.map_or(0, |s| s.len()));
    let armed_now = ARMED.lock().unwrap_or_else(|e| e.into_inner()).len();
    for (v, name) in ["private", "views"].iter().enumerate() {
        tracing::info!(
            "wc touch, last 10 s, {name}: {} pages armed after copy-back (+{} again after our own touch); the title touched {} distinct pages \
             ({} read faults, {} write faults); our own code touched {}; not armed {}, changed by others before a touch {}",
            n[ARMS][v],
            n[REARMS][v],
            pages[v],
            n[TITLE_READS][v],
            n[TITLE_WRITES][v],
            n[OURS][v],
            n[SKIPPED][v],
            n[LOST][v],
        );
    }
    tracing::info!("wc touch: armed now {armed_now} (one page in {})", sample());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aligned(base: usize) -> usize {
        base.next_multiple_of(PAGE)
    }

    /// An armed page faults on its first touch, gets its access back, and
    /// a touch by our own code queues it to be armed again.
    #[test]
    fn a_touch_opens_the_page_and_ours_is_armed_again() {
        // SAFETY: memory of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * PAGE, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as usize;
            assert_ne!(base, 0);
            let at = aligned(base);
            std::ptr::write_volatile(at as *mut u8, 7);
            install();
            assert!(arm_page(&mut ARMED.lock().unwrap(), at / PAGE, false));
            // The touch faults, the handler opens the page, the read goes on.
            assert_eq!(std::ptr::read_volatile(at as *const u8), 7);
            assert!(!ARMED.lock().unwrap().contains_key(&(at / PAGE)));
            assert!(REARM.lock().unwrap().contains(&(at / PAGE, false)), "a touch by the test's own image counts as ours");
            REARM.lock().unwrap().retain(|&(p, _)| p != at / PAGE);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }

    /// A read-only page gets read-only back, not read-write; a page whose
    /// protection changed after arming is left to the other handlers.
    #[test]
    fn a_page_gets_back_what_it_had_and_one_changed_is_not_ours() {
        // SAFETY: memory of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * PAGE, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as usize;
            assert_ne!(base, 0);
            let at = aligned(base);
            std::ptr::write_volatile(at as *mut u8, 9);
            let mut old = 0;
            assert_ne!(mem::VirtualProtect(at as *const c_void, PAGE, mem::PAGE_READONLY, &mut old), 0);
            install();
            assert!(arm_page(&mut ARMED.lock().unwrap(), at / PAGE, true));
            assert_eq!(std::ptr::read_volatile(at as *const u8), 9);
            let mut q: mem::MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            mem::VirtualQuery(at as *const c_void, &mut q, size_of_val(&q));
            assert_eq!(q.Protect, mem::PAGE_READONLY, "read-only, as it was");
            REARM.lock().unwrap().retain(|&(p, _)| p != at / PAGE);
            // Armed, then changed by someone else: still in the table, but
            // the fault (none here: it is readable) would not be ours.
            assert!(arm_page(&mut ARMED.lock().unwrap(), at / PAGE, true));
            assert_ne!(mem::VirtualProtect(at as *const c_void, PAGE, mem::PAGE_READWRITE, &mut old), 0);
            assert_eq!(std::ptr::read_volatile(at as *const u8), 9);
            // The next arming finds it open and forgets it, to arm it anew.
            assert!(!still_armed(&mut ARMED.lock().unwrap(), at / PAGE));
            assert!(!ARMED.lock().unwrap().contains_key(&(at / PAGE)));
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }
}
