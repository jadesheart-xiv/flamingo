//! Diagnostics (`FLAMINGO_WC_TOUCH=1`): how often the title touches, on
//! the CPU, the write-combined memory that shaders wrote and the copy-back
//! put into title memory. The question it answers: could that copy-back be
//! left until the CPU's first touch (a fetch behind a no-access guard)
//! instead of being made after every submission?
//!
//! After a copy-back into memory the title allocated write-combined
//! ([`crate::wcstat`] kind 0), the 64 KiB pages it wrote are made
//! no-access. The first touch faults into [`on_touch`], which gives the
//! page its access back and counts it:
//! - by the title (the faulting code, or the code a leaf copy routine
//!   returns to, lies outside our image): a read or a write, and the page
//!   stays open until the next copy-back writes it;
//! - by us (our uploads, compares and merges read the same memory): the
//!   page is armed again at the next copy-back, so our own work does not
//!   hide the title's touches for long. The counts are therefore a lower
//!   bound on the title's touches.
//!
//! Only private read-write memory is armed: physical-page views carry the
//! write watch's read-only guards (`crate::watch`), which this must not
//! disturb. A system call that reads or writes an armed page fails instead
//! of faulting; none of ours does (our copies try a plain copy first and
//! this handler runs ahead of theirs), a title's might.
//!
//! Measurement only: no copy-back is skipped, every byte stays right.

use std::collections::{BTreeSet, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory as mem;

const PAGE: usize = 64 << 10;

/// Pages made no-access and not touched since.
static ARMED: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());
/// Pages our own code touched: armed again at the next copy-back.
static REARM: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Pages the title touched in the current ten seconds.
static TITLE_PAGES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// Counters since the last line, in the order of [`NAMES`].
static N: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const ARMS: usize = 0;
const TITLE_READS: usize = 1;
const TITLE_WRITES: usize = 2;
const OURS: usize = 3;
const REARMS: usize = 4;
const SKIPPED_VIEW: usize = 5;
const SKIPPED_OTHER: usize = 6;

/// Our image: `[start, end)`.
static IMAGE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FLAMINGO_WC_TOUCH").is_some_and(|v| v == "1"))
}

/// The copy-back just wrote title memory `[from, to)`, write-combined.
pub(crate) fn arm(from: usize, to: usize) {
    if !on() || to <= from {
        return;
    }
    install();
    let rearm = std::mem::take(&mut *REARM.lock().unwrap_or_else(|e| e.into_inner()));
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    for page in rearm {
        if armed.contains(&page) {
            continue;
        }
        let mut old = 0;
        // SAFETY: a title page that was read-write when armed before; its
        // protection is put back below unless it still was.
        if unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_NOACCESS, &mut old) } != 0 {
            if old == mem::PAGE_READWRITE {
                armed.insert(page);
                N[REARMS].fetch_add(1, Relaxed);
            } else {
                // SAFETY: as above: what it was.
                unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, old, &mut old) };
            }
        }
    }
    let (first, last) = (from / PAGE, (to - 1) / PAGE);
    let mut page = first;
    while page <= last {
        if armed.contains(&page) {
            page += 1;
            continue;
        }
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { mem::VirtualQuery((page * PAGE) as *const c_void, &mut info, size_of_val(&info)) };
        let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize) / PAGE;
        if n == 0 || reach <= page {
            N[SKIPPED_OTHER].fetch_add(1, Relaxed);
            page += 1;
            continue;
        }
        let run_end = reach.min(last + 1);
        if info.Type != mem::MEM_PRIVATE || info.State != mem::MEM_COMMIT || info.Protect != mem::PAGE_READWRITE {
            let skipped = if info.Type == mem::MEM_MAPPED { SKIPPED_VIEW } else { SKIPPED_OTHER };
            N[skipped].fetch_add((run_end - page) as u64, Relaxed);
            page = run_end;
            continue;
        }
        // Pages already armed inside the run stay as they are.
        let mut at = page;
        while at < run_end {
            if armed.contains(&at) {
                at += 1;
                continue;
            }
            let mut end = at + 1;
            while end < run_end && !armed.contains(&end) {
                end += 1;
            }
            let mut old = 0;
            // SAFETY: committed private read-write title memory (checked
            // above, with the lock that the handler takes held).
            if unsafe { mem::VirtualProtect((at * PAGE) as *const c_void, (end - at) * PAGE, mem::PAGE_NOACCESS, &mut old) } != 0 {
                armed.extend(at..end);
                N[ARMS].fetch_add((end - at) as u64, Relaxed);
            }
            at = end;
        }
        page = run_end;
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
    {
        let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        if !armed.remove(&page) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let mut old = 0;
        // SAFETY: a page we made no-access; read-write is what it was.
        unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_READWRITE, &mut old) };
    }
    if ours(context.Rip as usize, context.Rsp as usize) {
        N[OURS].fetch_add(1, Relaxed);
        REARM.lock().unwrap_or_else(|e| e.into_inner()).push(page);
    } else {
        N[if access == 1 { TITLE_WRITES } else { TITLE_READS }].fetch_add(1, Relaxed);
        TITLE_PAGES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new).insert(page);
    }
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
        tracing::info!("wc touch: armed; write-combined pages the copy-back writes are made no-access until their first touch");
    });
}

/// The ten-second line ([`crate::wcstat`]'s logging thread).
pub(crate) fn report() {
    if !on() {
        return;
    }
    let n: Vec<u64> = N.iter().map(|c| c.swap(0, Relaxed)).collect();
    let pages = TITLE_PAGES.lock().unwrap_or_else(|e| e.into_inner()).take().map_or(0, |s| s.len());
    let armed_now = ARMED.lock().unwrap_or_else(|e| e.into_inner()).len();
    tracing::info!(
        "wc touch, last 10 s: {} pages armed after copy-back (+{} again after our own touch); the title touched {} distinct pages \
         ({} read faults, {} write faults, {:.1}% of the arms); our own code touched {}; not armed: {} pages of views, {} other; armed now {}",
        n[ARMS],
        n[REARMS],
        pages,
        n[TITLE_READS],
        n[TITLE_WRITES],
        100.0 * (n[TITLE_READS] + n[TITLE_WRITES]) as f64 / n[ARMS].max(1) as f64,
        n[OURS],
        n[SKIPPED_VIEW],
        n[SKIPPED_OTHER],
        armed_now
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An armed page faults on its first touch, gets its access back, and
    /// a touch by our own code queues it to be armed again.
    #[test]
    fn a_touch_opens_the_page_and_ours_is_armed_again() {
        // SAFETY: memory of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * PAGE, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as usize;
            assert_ne!(base, 0);
            // Page-aligned to 64 KiB for the test's arithmetic.
            let at = base.next_multiple_of(PAGE);
            let len = (base + 4 * PAGE - at) / PAGE * PAGE;
            assert!(len >= PAGE);
            std::ptr::write_volatile(at as *mut u8, 7);
            // `arm` itself checks `on()`; the test arms directly.
            install();
            {
                let mut armed = ARMED.lock().unwrap();
                let mut old = 0;
                assert_ne!(mem::VirtualProtect(at as *const c_void, PAGE, mem::PAGE_NOACCESS, &mut old), 0);
                armed.insert(at / PAGE);
            }
            // The touch faults, the handler opens the page, the read goes on.
            assert_eq!(std::ptr::read_volatile(at as *const u8), 7);
            assert!(!ARMED.lock().unwrap().contains(&(at / PAGE)));
            assert!(REARM.lock().unwrap().contains(&(at / PAGE)), "a touch by the test's own image counts as ours");
            REARM.lock().unwrap().retain(|&p| p != at / PAGE);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }
}
