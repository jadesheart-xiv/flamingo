# Lazy copy-back for write-combined title memory (design, not built)

Status: proposal. Measurements behind it: commits 731c861, 5bb425f, 3eddf87
(`FLAMINGO_WC_STAT`, `FLAMINGO_WC_TOUCH`), RTX 4070, Series S profile, the
opening area.

## What was measured

- 94-97% of all copy-back (GPU-written bytes put back into title memory,
  ~70-325 MB/s in gameplay) lands in the title's two `XMemVirtualAlloc`
  allocations with protect `0x2404`: write-combined plus a GPU access bit,
  3 GB together. The title itself marks this memory as written by the CPU and
  read by the GPU.
- With every page the copy-back wrote made no-access until its first touch,
  in gameplay, per 10 s:
  - physical-page views (most of it, sampled one page in 4): 1,108 / 4,736 /
    1,743 pages armed, **0 title reads, 0 title writes**;
  - private pages: **0 title reads**, 17-107 title writes on 7-8 pages;
  - our own code: 3,000-11,000 touches of the armed private pages, 0-141 of
    the views.
- So the title does not read back what shaders wrote there. The copy-back is
  kept alive by us.

## Why we touch it: the copy-back feedback loop

`write_mapped` writes title memory with a plain copy, which sets the kernel's
write-watch bit (private memory) or opens the guard (views). The next refresh
(`Shadow::update_free` → `watch::writes` → `copy_stale`) then takes those
pages for title writes and reads and uploads whole 64 KiB pages of what the
GPU itself just wrote. `shadow.rs` says so (around line 2753: "some dozens of
MB a frame"). `put_back_own` (`VICTORY_BACK_OWN_VIEW`, off) avoids it for views
only. The relay's merges (`relay_merge`, `card_merge*`, `back_piece`) and
`merge_under_writes` also read the title stretch before writing it.

## Phase A: stop our write-back counting as a title write (small)

For write-combined memory only, after the copy-back writes a page, record
that this page's current bytes are ours, so the next refresh does not re-read
and re-upload it:

- views: turn on the existing `put_back_own` path for write-combined shadows
  (it bumps `copied[]` without re-reading);
- private memory: after `write_mapped`, reset the page's write-watch bit
  (`ResetWriteWatch` on the written run) and bump `copied[]` to the current
  asking. A title write that races between our write and the reset would be
  lost: so leave out pages the title wrote in the last N askings (the 7-8 the
  touch count saw), which keep today's behaviour.

Gain expected: the "dozens of MB a frame" of re-uploads, and part of the
3,000-11,000 own touches. Correctness risk: low (the title data is the same
bytes we wrote). Switch: `FLAMINGO_WC_OWN_BACK=1`, off by default.

## Phase B: GPU-owned pages, fetched at the title's first touch (large)

Per 64 KiB page of a write-combined shadow, two states:

| State | Title memory | Copy-back | Guard |
|---|---|---|---|
| **title-current** | up to date | as today | none (today's watch only) |
| **GPU-owned** | stale | skipped | no-access |

- **Entering GPU-owned:** when a landing finds the page's pieces differing,
  instead of merging into title memory: keep the GPU's bytes in the host
  copy (system-memory copies already have them; card copies keep them in the
  CPU side, as the "title unmapped" branch at `shadow.rs:3639` does), advance
  the relay snapshot as a merge would, keep the page's range declared as
  newer than title (reuse `pending`/`Flights`, held open), and arm the page.
- **Leaving GPU-owned (title touch):** the vectored handler only looks the
  page up and posts it to the relay thread, then waits on a per-page event.
  The relay thread merges that one page the usual way (system memory: from the
  mapping, microseconds; card: one copy on the down lane plus a wait), marks
  `copied[]` current so the refresh does not take it for a title write,
  restores the protection and signals. The handler never takes `before`,
  `dirty`, `pending`, `CARDED` or a queue lock: the submitting thread can hold
  them while it faults.
- **Our own threads** must never fault on a GPU-owned page:
  - the refresh already skips declared (`pending`) ranges: keep that true;
  - later merges of the same page read the host copy, not title memory, while
    the page is GPU-owned (the title cannot have written it: a title write
    would have faulted and made it title-current first);
  - `merge_under_writes`, title buffer copies (`copy_mapped` with
    `title_dst`), indirect and descriptor reads of such memory: read from the
    host copy while GPU-owned, or fetch first;
  - a fault that still comes from our image restores the page synchronously
    only on the relay thread; elsewhere it is a bug to log and re-arm.
- **Other host copies of the same bytes** (nested buffers, windows,
  textures) learn of GPU writes today through the write watch after the
  copy-back. While a page is GPU-owned they must be told, or the page fetched
  before they refresh. Start with write-combined shadows that nothing else
  overlaps, and skip pages that are shared.

Expected gain: most of the ~28 ms a frame of copy-back and the matching
re-uploads, on 95% of the copy-back's bytes. Risks:

1. deadlock if a handler waits on something a faulting thread holds (keep the
   handler lock-free apart from its own map; never wait on our own threads);
2. system calls that read or write a GPU-owned page fail instead of faulting
   (none of ours; a title's might: log failures of known calls);
3. a fault waiting behind a whole landing on a busy relay thread (stall
   spikes; measure fault-to-signal latency);
4. aliases: a write through another view of the same physical page is not
   seen by a guard on one view (`watch` guards every view of a guarded page;
   do the same, or leave out pages with more than one view);
5. a title that does read this memory in another scene: the per-page state
   falls back to today's path at the first touch, so cost, not breakage.

Switch: `FLAMINGO_WC_LAZY=1`, off by default, needs `FLAMINGO_WC_STAT`'s
registry.

## How to tell it works

Same scene, same settings, three runs each, off vs on:

- fps and the frame line's "copying back" ms and MB;
- refresh MB per frame ("copies ... MB" in "preparing");
- faults per second and fault-to-signal latency (new counters);
- picture: no new flicker, missing geometry or stale UI (what copy-back to
  write-combined memory feeds, if anything, shows up here first).

Phase A ships alone if it pays; Phase B only behind its switch until it
holds up in more than the opening area.

## Result of phase A (built: `FLAMINGO_WC_OWN_BACK`, off by default)

One run, `FLAMINGO_WC_OWN_BACK=alt` (twenty seconds on, twenty off, the
same spot), RTX 4070, 15 steady samples each way:

| | on | off |
|---|---|---|
| median fps | 9.6 | 9.9 |
| upload ("copies" in preparing) per frame | 128 MB | 141 MB |
| copy-back per frame | 32.2 ms | 26.1 ms |

18 GB stored without counting as title writes, ~405,000 pages noted current
(not taken again), 2,216 stretches left to the old way because the title
had written there, 0 races, 0 errors. The saving was ~13 MB a frame of
upload, less than expected (most write-combined copy-back lands in views,
which this does not cover), and the extra kernel calls and reads per
stretch cost ~6 ms of copy-back: no net gain. The re-upload loop on private
memory is not what holds the frame here. Asking the bits once per range
instead of per stretch could cut the cost; phase B, which removes the
copy-back itself, is the larger lever.

## Result of phase B, first step (built: `FLAMINGO_WC_LAZY`, off by default)

Built for private memory, copies in the PC's memory, the relay's merges
(`crates/victory-gpu/src/wclazy.rs`). Unattended runs with scripted keys,
RTX 4070, the character standing in the first area, `alt` twenty seconds
on and off:

| run | median fps on | median fps off | MB not written to title memory |
|---|---|---|---|
| B1 as designed | 10.4 | 11.4 | 2-7 MB/s of ~400 |
| B1 + strikes (a page our code fetched twice is not owned again) | 11.3 | 11.3 | 0 |

No errors, no device loss in any run. The mechanism works (pages owned,
fetched at their first touch, given back to their copy's bytes), but in
gameplay ~90% of the pages owned were fetched straight back **by our own
code**. The call chains, unwound from the faults and looked up in the PDB:

- `relay_land → Shadow::relay_merge → read_into` (~45%): another copy
  merging into a page a different copy owns;
- `Host::submit → Shadow::update → update_free → copy → copy_mapped`
  (~50%): another copy's refresh re-reading the page (the journal tells
  every other copy the page changed, as it must);
- `stale_under_flights → merge_under_writes` (a few %).

So the write-combined memory is covered by more than one host copy (title
buffers and the 4 MiB windows over the same bytes), and the others read
title memory every frame. With strikes, ~10,000 page owns per 10 s are
refused: nearly every page is such a page. Skipping copy-back cannot pay
while the other copies take their bytes from title memory.

What would: copies of the same memory synced with each other instead of
through title memory (a refresh of a page another copy owns taking it from
that copy's mapping, not from the title's address; merges into such a page
going to the owner). That is a change to how copies overlap, not to the
copy-back, and the next thing to look at before any more of phase B.
