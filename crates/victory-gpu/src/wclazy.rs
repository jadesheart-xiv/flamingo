//! Lazy copy-back for write-combined title memory (`FLAMINGO_WC_LAZY=1`, or
//! `alt`: twenty seconds on, twenty off). docs/wc-lazy-copyback.md, phase
//! B, first step (B1): private memory, copies in the PC's memory, the
//! relay's merges.
//!
//! The title does not read back what shaders wrote into the memory it
//! allocated write-combined (measured: `FLAMINGO_WC_TOUCH`), yet every
//! landing merges it into title memory. Here a 64 KiB page of such memory
//! becomes **GPU-owned**: its title bytes are left stale, the page is made
//! no-access, and the host copy (which the GPU writes directly) holds the
//! page's bytes. Later landings advance the relay's snapshot and skip the
//! title write (`Shadow::relay_merge`). The first touch of the page, by
//! the title or by us, faults into [`on_fault`], which copies the host
//! copy's bytes into the page ("fetch"), gives the page its access back and
//! forgets it: the page is title-current again, and the next landing merges
//! it as before (and may make it GPU-owned again).
//!
//! When a page may become GPU-owned (`Shadow::lazy_enter`): right after a
//! merge stored without leaving our own write-watch bits
//! (`store_unwatched`, no race), when the copy is current for the page and
//! no 4 KiB page of it was written since the last asking: then title memory
//! equals the copy outside what the GPU wrote, and a fetch puts back
//! exactly what the merges would have. After arming, the bits are asked
//! once more: a title write that landed meanwhile un-arms the page, with
//! no fetch.
//!
//! One owner per page: a page another copy owns is fetched before this
//! one merges into it (`own` refuses; the caller's read faults and
//! fetches). A copy dropped while it owns pages fetches them first
//! ([`shadow_gone`]).
//!
//! Known gap: between the fault handler giving the page its access back
//! and its copy finishing (a few microseconds), another thread touching the
//! same page without faulting sees the stale bytes. The title was measured
//! not to read this memory.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory as mem;

pub(crate) const PAGE: usize = 64 << 10;

/// The copy that owns a page: an identity (the `Shadow`'s address), its
/// mapping and its title range.
#[derive(Clone, Copy)]
pub(crate) struct Owner {
    pub(crate) id: usize,
    pub(crate) data: usize,
    pub(crate) start: usize,
    pub(crate) len: usize,
}

static OWNED: Mutex<BTreeMap<usize, Owner>> = Mutex::new(BTreeMap::new());
/// Pages released lately: a fault there raced with the release and only
/// needs to run again.
static RELEASED: [AtomicUsize; 64] = [const { AtomicUsize::new(usize::MAX) }; 64];
static RELEASED_AT: AtomicUsize = AtomicUsize::new(0);

/// Pages our own code fetched back, and how often. Another copy of the same
/// memory (a buffer and a window over it) keeps reading such a page, so
/// owning it costs a fault and gains nothing: after [`STRIKES`] it is not
/// owned again.
static STRUCK: Mutex<BTreeMap<usize, u8>> = Mutex::new(BTreeMap::new());
/// `FLAMINGO_WC_LAZY_STRIKES` (2): fetches by our own code after which a
/// page is left to the old way; 0 never.
fn strikes() -> u8 {
    static N: OnceLock<u8> = OnceLock::new();
    *N.get_or_init(|| std::env::var("FLAMINGO_WC_LAZY_STRIKES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Since the start: per page, which of our paths fetched it, as bits: 1 the
/// relay thread (another copy merging into the page), 2 any other thread
/// (a refresh at submission reading it, mostly). Pages only ever fetched by
/// refreshes could be refreshed from the owner's mapping instead.
static PATHS: Mutex<BTreeMap<usize, u8>> = Mutex::new(BTreeMap::new());

/// Counters since the last line.
static N: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
/// Pages not owned for their strikes.
const STRUCK_OUT: usize = 9;
const SKIPPED: usize = 0;
const SKIPPED_BYTES: usize = 1;
const OWNED_NEW: usize = 2;
const FETCH_TITLE: usize = 3;
const FETCH_OURS: usize = 4;
const NOT_CLEAN: usize = 5;
const UNARMED: usize = 6;
const REFUSED: usize = 7;
const RETRIED: usize = 8;

/// Our image: `[start, end)`, to tell our touches from the title's.
static IMAGE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// 0 off, 1 on, 2 in turn.
fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("FLAMINGO_WC_LAZY").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    })
}

/// Whether the switch is set at all (the registry of write-combined
/// memory must run from the start).
pub(crate) fn wanted() -> bool {
    mode() != 0
}

/// Whether pages are made GPU-owned and skipped now. Off (in `alt`'s off
/// periods too): pages already owned stay so until touched; a merge into
/// one reads it, which fetches it.
pub(crate) fn on() -> bool {
    match mode() {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
            static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
            let period = EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20;
            let on = period % 2 == 0;
            if LAST.swap(period, Relaxed) != period {
                tracing::info!("wc lazy (FLAMINGO_WC_LAZY=alt): {} from now", if on { "ON" } else { "OFF" });
            }
            on
        }
    }
}

/// Whether `page` is owned by the copy `id`.
pub(crate) fn owned_by(page: usize, id: usize) -> bool {
    OWNED.lock().unwrap_or_else(|e| e.into_inner()).get(&page).is_some_and(|o| o.id == id)
}

/// A landing left page `page` stale on purpose: `n` bytes not written.
pub(crate) fn skipped(n: usize) {
    N[SKIPPED].fetch_add(1, Relaxed);
    N[SKIPPED_BYTES].fetch_add(n as u64, Relaxed);
}

/// Whether no 4 KiB page of `[lo, hi)` was written since the last asking
/// (asked without reset). False if the memory is not write-watched.
fn clean(lo: usize, hi: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    let (k0, k1) = (lo & !4095, hi.next_multiple_of(4096));
    let mut found = vec![0usize; (k1 - k0) / 4096];
    let (mut count, mut granularity) = (found.len(), 0u32);
    // SAFETY: an out array of `count` entries; flags 0 asks without
    // resetting.
    let failed = unsafe { GetWriteWatch(0, k0 as *const _, k1 - k0, found.as_mut_ptr().cast(), &mut count, &mut granularity) };
    failed == 0 && count == 0
}

/// The part of `page` that `o` covers.
fn covered(page: usize, o: &Owner) -> (usize, usize) {
    ((page * PAGE).max(o.start), ((page + 1) * PAGE).min(o.start + o.len))
}

/// Makes `page` GPU-owned by `o`: the caller has just stored the page's
/// merge without leaving our bits and found the copy current. True if the
/// page is owned (now or already, by `o`).
pub(crate) fn own(page: usize, o: Owner) -> bool {
    install();
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(other) = owned.get(&page) {
        if other.id == o.id {
            return true;
        }
        N[REFUSED].fetch_add(1, Relaxed);
        return false;
    }
    let limit = strikes();
    if limit != 0 && STRUCK.lock().unwrap_or_else(|e| e.into_inner()).get(&page).is_some_and(|&s| s >= limit) {
        N[STRUCK_OUT].fetch_add(1, Relaxed);
        return false;
    }
    let (lo, hi) = covered(page, &o);
    if lo >= hi || !clean(lo, hi) {
        N[NOT_CLEAN].fetch_add(1, Relaxed);
        return false;
    }
    let mut old = 0;
    // SAFETY: committed private title memory (the caller checked it is not
    // view memory; a failure or another protection is put back below).
    if unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_NOACCESS, &mut old) } == 0 {
        return false;
    }
    if old != mem::PAGE_READWRITE {
        // SAFETY: as above: what it was.
        unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, old, &mut old) };
        return false;
    }
    // A title write between the check and the guard: its bytes are in title
    // memory and not in the copy. Un-armed, nothing fetched.
    if !clean(lo, hi) {
        // SAFETY: the page we just made no-access; read-write it was.
        unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_READWRITE, &mut old) };
        N[UNARMED].fetch_add(1, Relaxed);
        return false;
    }
    owned.insert(page, o);
    N[OWNED_NEW].fetch_add(1, Relaxed);
    true
}

/// Gives `page` its access back and copies the owner's bytes into it.
/// `OWNED`'s lock is held: no one else arms or releases meanwhile.
fn release(page: usize, o: &Owner) {
    let mut old = 0;
    // SAFETY: a page we made no-access; read-write is what it was.
    unsafe { mem::VirtualProtect((page * PAGE) as *const c_void, PAGE, mem::PAGE_READWRITE, &mut old) };
    let (lo, hi) = covered(page, o);
    if lo < hi {
        // SAFETY: the owner's mapping covers its whole title range (alive:
        // a copy releases its pages before it goes, `shadow_gone`); the
        // title page is read-write now.
        unsafe { std::ptr::copy_nonoverlapping((o.data + (lo - o.start)) as *const u8, lo as *mut u8, hi - lo) };
    }
    let slot = RELEASED_AT.fetch_add(1, Relaxed) % RELEASED.len();
    RELEASED[slot].store(page, Relaxed);
}

thread_local! {
    /// Set while [`refresh_from_owners`] holds `OWNED`'s lock: a fault on
    /// this thread then is not one of ours (owned pages are not read from
    /// title memory there), and the handler must not wait for the lock.
    static IN_REFRESH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Pages refreshed from their owner's mapping, and their bytes, since the
/// last line; pages fetched for a refresh because the owner does not cover
/// the part asked for.
static FROM_OWNER: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// For a refresh of the copy `id` (`Shadow::copy`): copies title memory
/// `[from, to)` into `dst` (the copy's mapping at `from`), taking each page
/// another copy owns from that copy's mapping instead of from the stale,
/// guarded title page; a page `id` owns itself is left as the copy holds
/// it (newer than title memory). False with nothing done if no page there
/// is owned (the caller copies as before).
///
/// Overlapping copies (title buffers and the windows over them) refresh
/// each other's GPU-owned pages at every submission, and each such read
/// used to fetch the page back (docs/wc-lazy-copyback.md, phase B1).
pub(crate) fn refresh_from_owners(id: usize, from: usize, to: usize, dst: *mut u8) -> bool {
    if from >= to {
        return false;
    }
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let (first, last) = (from / PAGE, (to - 1) / PAGE);
    let pages: Vec<(usize, Owner)> = owned.range(first..=last).map(|(&p, &o)| (p, o)).collect();
    if pages.is_empty() {
        return false;
    }
    IN_REFRESH.with(|f| f.set(true));
    let title = |a: usize, b: usize| {
        if a < b {
            // SAFETY: `dst` maps `[from, to)`; `[a, b)` is inside it.
            crate::host::copy_mapped(unsafe { dst.add(a - from) }, a as *const u8, b - a, false);
        }
    };
    let mut at = from;
    for (page, o) in pages {
        let (lo, hi) = ((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
        title(at, lo);
        let (clo, chi) = covered(page, &o);
        if o.id == id {
            // Our own page: the copy holds what title memory will get.
        } else if clo <= lo && hi <= chi {
            // SAFETY: the owner's mapping covers `[lo, hi)` (inside its
            // range) and stays alive while `OWNED`'s lock is held
            // (`shadow_gone`); `dst` maps `[from, to)`.
            unsafe { std::ptr::copy_nonoverlapping((o.data + (lo - o.start)) as *const u8, dst.add(lo - from), hi - lo) };
            FROM_OWNER[0].fetch_add(1, Relaxed);
            FROM_OWNER[1].fetch_add((hi - lo) as u64, Relaxed);
        } else {
            // The owner covers only part of what is asked for: fetched,
            // then read from title memory as before.
            owned.remove(&page);
            release(page, &o);
            title(lo, hi);
            FROM_OWNER[2].fetch_add(1, Relaxed);
        }
        at = hi;
    }
    title(at, to);
    IN_REFRESH.with(|f| f.set(false));
    true
}

/// The copy `id` goes: every page it owns is fetched first.
pub(crate) fn shadow_gone(id: usize) {
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if owned.is_empty() {
        return;
    }
    let pages: Vec<usize> = owned.iter().filter(|(_, o)| o.id == id).map(|(&p, _)| p).collect();
    for page in pages {
        if let Some(o) = owned.remove(&page) {
            release(page, &o);
        }
    }
}

/// Our touches that fetched a page, since the last line: by thread name
/// (the title's threads have none), and by call chain in our image (the
/// faulting thread's stack unwound from the fault's context, up to five
/// frames in our image), as offsets from the image's base for a symbol
/// lookup in the build's PDB.
static WHO: Mutex<Option<(BTreeMap<String, u64>, BTreeMap<String, u64>)>> = Mutex::new(None);

/// Up to `n` return addresses of the thread in `context`, unwound with the
/// image's unwind tables.
fn unwind(context: &windows_sys::Win32::System::Diagnostics::Debug::CONTEXT, n: usize) -> Vec<usize> {
    use windows_sys::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER};
    let mut c = *context;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let pc = c.Rip;
        if pc == 0 {
            break;
        }
        out.push(pc as usize);
        let mut base = 0u64;
        // SAFETY: plain lookups and an unwind of a copy of the context.
        unsafe {
            let entry = RtlLookupFunctionEntry(pc, &mut base, std::ptr::null_mut());
            if entry.is_null() {
                // A leaf: its return address is on top of the stack.
                c.Rip = (c.Rsp as *const u64).read_unaligned();
                c.Rsp += 8;
            } else {
                let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
                RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, pc, entry, &mut c, &mut data, &mut frame, std::ptr::null_mut());
            }
        }
    }
    out
}

fn note_who(context: &windows_sys::Win32::System::Diagnostics::Debug::CONTEXT) {
    let (start, end) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
    let chain: Vec<String> = unwind(context, 12)
        .into_iter()
        .filter(|a| (start..end).contains(a))
        .take(5)
        .map(|a| format!("+{:#x}", a - start))
        .collect();
    let name = std::thread::current().name().unwrap_or("(a title thread)").to_owned();
    let mut who = WHO.lock().unwrap_or_else(|e| e.into_inner());
    let (names, places) = who.get_or_insert_with(Default::default);
    *names.entry(name).or_default() += 1;
    *places.entry(chain.join(" < ")).or_default() += 1;
}

fn ours(rip: usize, rsp: usize) -> bool {
    let (start, end) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
    if (start..end).contains(&rip) {
        return true;
    }
    // SAFETY: the faulting thread's stack pointer, at its top: readable.
    let ret = unsafe { (rsp as *const usize).read_unaligned() };
    (start..end).contains(&ret)
}

unsafe extern "system" fn on_fault(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH};
    // SAFETY: the system passes the exception being dispatched and the
    // context it will continue with.
    let (r, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION || r.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let page = r.ExceptionInformation[1] / PAGE;
    // This thread holds `OWNED`'s lock in a refresh, which reads no owned
    // page from title memory: the fault is someone else's (an unmapped
    // page the plain copy recovers from), and waiting for the lock here
    // would never end.
    if IN_REFRESH.with(|f| f.get()) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    {
        let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(o) = owned.remove(&page) {
            release(page, &o);
            drop(owned);
            let (rip, rsp) = (context.Rip as usize, context.Rsp as usize);
            if ours(rip, rsp) {
                N[FETCH_OURS].fetch_add(1, Relaxed);
                let mut struck = STRUCK.lock().unwrap_or_else(|e| e.into_inner());
                let s = struck.entry(page).or_default();
                *s = s.saturating_add(1);
                drop(struck);
                let bit = if std::thread::current().name() == Some("victory-relay") { 1 } else { 2 };
                *PATHS.lock().unwrap_or_else(|e| e.into_inner()).entry(page).or_default() |= bit;
                note_who(context);
            } else {
                N[FETCH_TITLE].fetch_add(1, Relaxed);
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    // Faulted while another thread released it: it is open now.
    if RELEASED.iter().any(|p| p.load(Relaxed) == page) {
        N[RETRIED].fetch_add(1, Relaxed);
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// Our handler, ahead of the write watch's and the plain copies' (each
/// added at the front of the line: theirs first, then ours), so that a
/// fault on an owned page is fetched before they take a read fault inside
/// their copy for an unmapped page.
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
            windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(1, Some(on_fault));
        }
        tracing::info!("wc lazy: write-combined private pages the GPU wrote stay stale until their first touch");
    });
}

/// The ten-second line ([`crate::wcstat`]'s logging thread).
pub(crate) fn report() {
    if !wanted() {
        return;
    }
    let n: Vec<u64> = N.iter().map(|c| c.swap(0, Relaxed)).collect();
    let owned = OWNED.lock().unwrap_or_else(|e| e.into_inner()).len();
    tracing::info!(
        "wc lazy, last 10 s: {} merges skipped ({:.1} MB not written to title memory); {} pages made GPU-owned, {} owned now; \
         fetched at a first touch: {} by the title, {} by our own code; not owned: {} not clean, {} un-armed (a title write raced), \
         {} owned by another copy, {} struck out (our code kept fetching them); {} faults retried after a release",
        n[SKIPPED],
        n[SKIPPED_BYTES] as f64 / 1e6,
        n[OWNED_NEW],
        owned,
        n[FETCH_TITLE],
        n[FETCH_OURS],
        n[NOT_CLEAN],
        n[UNARMED],
        n[REFUSED],
        n[STRUCK_OUT],
        n[RETRIED],
    );
    tracing::info!(
        "wc lazy, refreshes of other copies, last 10 s: {} pages ({:.1} MB) taken from their owner's mapping, {} fetched (owner covers part)",
        FROM_OWNER[0].swap(0, Relaxed),
        FROM_OWNER[1].swap(0, Relaxed) as f64 / 1e6,
        FROM_OWNER[2].swap(0, Relaxed)
    );
    {
        let paths = PATHS.lock().unwrap_or_else(|e| e.into_inner());
        let mut by = [0usize; 4];
        for &bits in paths.values() {
            by[(bits & 3) as usize] += 1;
        }
        if paths.len() > 0 {
            tracing::info!(
                "wc lazy, pages our code fetched since the start: {} only by merges of another copy (relay thread), {} only by \
                 refreshes and other reads, {} by both",
                by[1],
                by[2],
                by[3]
            );
        }
    }
    if let Some((names, places)) = WHO.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        let mut places: Vec<_> = places.into_iter().collect();
        places.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        let names: Vec<String> = names.iter().take(5).map(|(n, c)| format!("{n}: {c}")).collect();
        let places: Vec<String> = places.iter().take(6).map(|(a, c)| format!("[{a}]: {c}")).collect();
        tracing::info!("wc lazy, our fetching touches by thread: {}; by call chain (image offsets): {}", names.join(", "), places.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's write watching (not in windows-sys' memory module).
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// A GPU-owned page faults at its first touch, gets the owner's bytes
    /// and its access back; a page written since the last asking is not
    /// owned.
    #[test]
    fn an_owned_page_is_fetched_at_its_first_touch() {
        // SAFETY: memory of the test's own.
        unsafe {
            let title = mem::VirtualAlloc(std::ptr::null(), 2 * PAGE, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE) as usize;
            assert_ne!(title, 0);
            let page = title / PAGE;
            for at in (0..2 * PAGE).step_by(4096) {
                std::ptr::write_volatile((title + at) as *mut u8, 1);
            }
            // The copy: what the GPU wrote (2s), the title's stale (1s).
            let copy = vec![2u8; 2 * PAGE];
            let o = Owner { id: 0x5eed, data: copy.as_ptr() as usize, start: title, len: 2 * PAGE };
            // Written since the last asking: not owned.
            assert!(!own(page, o));
            let mut found = vec![0usize; 2 * PAGE / 4096];
            let (mut count, mut g) = (found.len(), 0u32);
            windows_sys::Win32::System::Memory::GetWriteWatch(1, title as *const _, 2 * PAGE, found.as_mut_ptr().cast(), &mut count, &mut g);
            // Clean now: owned, guarded.
            assert!(own(page, o));
            assert!(owned_by(page, 0x5eed));
            // The first touch fetches the copy's bytes.
            assert_eq!(std::ptr::read_volatile((title + 100) as *const u8), 2);
            assert!(!owned_by(page, 0x5eed));
            // The second page was never owned: the title's bytes.
            assert_eq!(std::ptr::read_volatile((title + PAGE) as *const u8), 1);
            // Owned again, then the copy goes: fetched before.
            let mut found = vec![0usize; 2 * PAGE / 4096];
            let (mut count, mut g) = (found.len(), 0u32);
            windows_sys::Win32::System::Memory::GetWriteWatch(1, title as *const _, 2 * PAGE, found.as_mut_ptr().cast(), &mut count, &mut g);
            assert!(own(page + 1, o));
            shadow_gone(0x5eed);
            assert!(!owned_by(page + 1, 0x5eed));
            assert_eq!(std::ptr::read_volatile((title + PAGE + 100) as *const u8), 2);
            mem::VirtualFree(title as *mut c_void, 0, mem::MEM_RELEASE);
            drop(copy);
        }
    }

    /// Another copy's refresh of an owned page takes the owner's bytes,
    /// with no fault and the page still owned; the parts of the range that
    /// are not owned come from title memory.
    #[test]
    fn another_copy_refreshes_an_owned_page_from_its_owner() {
        // SAFETY: memory of the test's own.
        unsafe {
            let title = mem::VirtualAlloc(std::ptr::null(), 2 * PAGE, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE) as usize;
            assert_ne!(title, 0);
            for at in (0..2 * PAGE).step_by(4096) {
                std::ptr::write_volatile((title + at) as *mut u8, 1);
            }
            let mut found = vec![0usize; 2 * PAGE / 4096];
            let (mut count, mut g) = (found.len(), 0u32);
            windows_sys::Win32::System::Memory::GetWriteWatch(1, title as *const _, 2 * PAGE, found.as_mut_ptr().cast(), &mut count, &mut g);
            let page = title / PAGE;
            let owner_copy = vec![2u8; PAGE];
            let o = Owner { id: 0xa11ce, data: owner_copy.as_ptr() as usize, start: title, len: PAGE };
            assert!(own(page, o));
            // Copy B covers both pages: the first from its owner, the second
            // from title memory.
            let mut b = vec![0u8; 2 * PAGE];
            assert!(refresh_from_owners(0xb0b, title, title + 2 * PAGE, b.as_mut_ptr()));
            assert_eq!(b[100], 2, "the owned page comes from the owner's copy");
            assert_eq!(b[PAGE], 1, "the page not owned comes from title memory");
            assert!(owned_by(page, 0xa11ce), "still owned: no fault, no fetch");
            // The owner's own refresh leaves its bytes as they are.
            let mut own_copy = vec![7u8; PAGE];
            assert!(refresh_from_owners(0xa11ce, title, title + PAGE, own_copy.as_mut_ptr()));
            assert_eq!(own_copy[100], 7);
            shadow_gone(0xa11ce);
            mem::VirtualFree(title as *mut c_void, 0, mem::MEM_RELEASE);
            drop(owner_copy);
        }
    }
}
