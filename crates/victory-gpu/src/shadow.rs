//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::memstat::{self, Kind};
use crate::watch::PAGE;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// What the host buffer is for and its size, as counted in [`memstat`].
    kind: Kind,
    bytes: u64,
}

impl Drop for Shadow {
    fn drop(&mut self) {
        memstat::free(self.kind, self.bytes);
    }
}

/// One submission's pre-GPU copy of the bytes its shaders may write, from
/// offset `base` of a shadow: copy-back writes only bytes the GPU changed.
/// Owned by the submission (not the shadow), so overlapping submissions
/// cannot replace or consume each other's baselines. Its buffer goes back
/// to the pool when dropped.
pub(crate) struct Snapshot {
    base: usize,
    bytes: Vec<u8>,
    /// The buffer's capacity as counted in [`memstat`].
    counted: u64,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        memstat::free(Kind::Snapshot, self.counted);
        return_snapshot_buf(std::mem::take(&mut self.bytes));
    }
}

/// Reusable snapshot buffers (kept warm to avoid repeated page faults).
static SNAPSHOT_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// At most this many pooled snapshot buffers ...
const POOL_BUFFERS: usize = 8;
/// ... of at most this many bytes in all; past it the largest go first, so
/// one huge unordered-access buffer cannot pin its snapshot's memory.
const POOL_BYTES: usize = 64 << 20;

/// A snapshot buffer of at least `cap` bytes: the smallest pooled one that
/// fits, else a new one. `None` if a new one cannot be allocated.
fn take_snapshot_buf(cap: usize) -> Option<Vec<u8>> {
    let mut pool = SNAPSHOT_POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= cap)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => {
            let v = pool.swap_remove(i);
            memstat::free(Kind::SnapshotPool, v.capacity() as u64);
            v
        }
        None => {
            drop(pool);
            let mut v = Vec::new();
            v.try_reserve_exact(cap).ok()?;
            v
        }
    };
    v.clear();
    Some(v)
}

fn return_snapshot_buf(mut v: Vec<u8>) {
    if v.capacity() == 0 {
        return;
    }
    v.clear();
    let mut pool = SNAPSHOT_POOL.lock().unwrap_or_else(|e| e.into_inner());
    memstat::alloc(Kind::SnapshotPool, v.capacity() as u64);
    pool.push(v);
    pool.sort_by_key(|b| b.capacity());
    let mut total: usize = pool.iter().map(Vec::capacity).sum();
    while pool.len() > POOL_BUFFERS || total > POOL_BYTES {
        let Some(largest) = pool.pop() else { break };
        total -= largest.capacity();
        memstat::free(Kind::SnapshotPool, largest.capacity() as u64);
    }
}

/// Exact contiguous byte ranges `(start, end)` where `before` and `after`
/// differ, using fast chunk comparison to skip unchanged memory.
fn diff_runs(before: &[u8], after: &[u8]) -> Vec<(usize, usize)> {
    debug_assert_eq!(before.len(), after.len());
    let len = before.len();
    let mut runs = Vec::new();
    let mut i = 0;
    const CHUNK: usize = 64;
    while i < len {
        // Fast skip of identical 64-byte chunks.
        while i + CHUNK <= len && before[i..i + CHUNK] == after[i..i + CHUNK] {
            i += CHUNK;
        }
        while i < len && before[i] == after[i] {
            i += 1;
        }
        if i >= len {
            break;
        }
        let start = i;
        while i < len && before[i] != after[i] {
            i += 1;
        }
        runs.push((start, i));
    }
    runs
}

/// Merges overlapping or adjacent half-open ranges `[start, end)`.
fn merge_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<(usize, usize)> = ranges.iter().copied().filter(|&(a, b)| a < b).collect();
    sorted.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (a, b) in sorted {
        match merged.last_mut() {
            Some((_, end)) if a <= *end => *end = (*end).max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    /// A copy of `[start, start + len)` in `buffer` (`bytes` long, mapped
    /// at `data`, at host GPU address `gpu`), counted as `kind`.
    fn new(
        (buffer, data, gpu, bytes): (ID3D12Resource, *mut u8, u64, u64),
        kind: Kind,
        start: u64,
        len: u64,
    ) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        memstat::alloc(kind, bytes);
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![0; pages as usize]),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            kind,
            bytes,
        }
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, changed byte
    /// ranges are copied back into title memory, where the title reads them;
    /// until then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At submission: a submitted list's shaders write this range. Exact
    /// duplicates share one copy-back and therefore need only one pending
    /// entry to suppress refreshes until that copy-back completes.
    pub(crate) fn writing(&self, address: usize, len: usize) {
        let range = (address, address + len);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.contains(&range) {
            pending.push(range);
        }
    }

    /// Before GPU execution: snapshots the union of `ranges` `(address, len)`
    /// inside this shadow so [`Shadow::copy_back_ranges`] writes back only
    /// bytes the GPU actually modified. `None` if no range is inside, or if
    /// the snapshot cannot be allocated (copy-back then writes whole ranges,
    /// as it did before snapshots).
    pub(crate) fn snapshot_ranges(&self, ranges: &[(usize, usize)]) -> Option<Snapshot> {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let mut min_off = usize::MAX;
        let mut max_off = 0usize;
        for &(address, len) in ranges {
            let from = address.clamp(start, end);
            let to = address.saturating_add(len).clamp(from, end);
            if from < to {
                min_off = min_off.min(from - start);
                max_off = max_off.max(to - start);
            }
        }
        if min_off >= max_off {
            return None;
        }
        let span = max_off - min_off;
        let Some(mut buf) = take_snapshot_buf(span) else {
            tracing::warn!(
                span,
                "no memory for a copy-back snapshot; whole ranges go back"
            );
            return None;
        };
        // SAFETY: `[min_off, max_off)` lies inside this shadow's persistent mapping.
        let src = unsafe { std::slice::from_raw_parts(self.data.add(min_off), span) };
        buf.extend_from_slice(src);
        let counted = buf.capacity() as u64;
        memstat::alloc(Kind::Snapshot, counted);
        Some(Snapshot {
            base: min_off,
            bytes: buf,
            counted,
        })
    }

    /// After the submission ran: compares each merged range in `ranges`
    /// `(address, len)` against its pre-submission `snapshot` and copies back
    /// only the exact byte runs that changed, preventing redundant kernel
    /// copies and preserving concurrent title CPU writes to untouched bytes.
    /// Returns the total changed bytes copied back.
    /// Ranges outside the snapshot (or with none) go back whole.
    pub(crate) fn copy_back_ranges(
        &self,
        ranges: &[(usize, usize)],
        snapshot: Option<&Snapshot>,
    ) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let clamped: Vec<(usize, usize)> = ranges
            .iter()
            .filter_map(|&(address, len)| {
                let from = address.clamp(start, end);
                let to = address.saturating_add(len).clamp(from, end);
                (from < to).then_some((from, to))
            })
            .collect();
        let merged = merge_ranges(&clamped);
        let mut changed_total = 0usize;

        for &(from, to) in &merged {
            let from_off = from - start;
            let to_off = to - start;
            // SAFETY: `[from_off, to_off)` lies inside this shadow's mapping.
            let after = unsafe { std::slice::from_raw_parts(self.data.add(from_off), to - from) };
            let mut range_changed = 0usize;
            if let Some(Snapshot { base, bytes, .. }) = snapshot
                && from_off >= *base
                && to_off <= base + bytes.len()
            {
                let before = &bytes[from_off - base..to_off - base];
                for (run_start, run_end) in diff_runs(before, after) {
                    let run_addr = from + run_start;
                    let run_len = run_end - run_start;
                    crate::watch::writing(run_addr, run_len);
                    crate::host::write_mapped(
                        run_addr as *mut u8,
                        after[run_start..].as_ptr(),
                        run_len,
                    );
                    range_changed += run_len;
                }
            } else {
                crate::watch::writing(from, to - from);
                crate::host::write_mapped(from as *mut u8, after.as_ptr(), to - from);
                range_changed = to - from;
            }
            changed_total += range_changed;
            // Do not advance `copied` for this page. Diff-only copy-back
            // preserves concurrent title writes at other offsets, so the
            // whole page is not known to be synchronized with the shadow.
            // A later refresh must still be allowed to copy those writes.
        }

        self.release(ranges);
        changed_total
    }

    /// The shader writes of `ranges` `(address, len)` are over (copied back
    /// or abandoned): refreshes may copy title memory there again.
    pub(crate) fn release(&self, ranges: &[(usize, usize)]) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for &(address, len) in ranges {
            if let Some(i) = pending.iter().position(|&r| r == (address, address + len)) {
                pending.swap_remove(i);
            }
        }
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_span(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_span(at, to);
            }
            return bytes;
        }
        self.update_span(from, to)
    }

    /// Copies stale pages of `[from, to)` using write watching when available.
    fn update_span(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                *at = now;
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy(first * PAGE, page * PAGE);
            }
        }
        if let Some(first) = run {
            bytes += self.copy(first * PAGE, (last + 1) * PAGE);
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
/// Also prunes any aligned sub-buffer shadows inside its address range so
/// freed buffers do not leak VRAM.
pub(crate) fn unregister(resource: usize, start: u64) {
    let removed = buffers().remove(&(start, resource));
    if let Some(entry) = removed {
        let end = entry.end;
        let mut aligned = ALIGNED.lock().unwrap_or_else(|e| e.into_inner());
        aligned.retain(|&(a_start, a_len), _| a_start + a_len <= start || a_start >= end);
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let bytes = len.next_multiple_of(1 << 16);
        let (buffer, data) = host()?.shared_buffer(bytes)?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Arc::new(Shadow::new(
            (buffer, data, gpu, bytes),
            Kind::Buffer,
            key.0,
            len,
        )));
    }
    entry.shadow.clone()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// Dedicated aligned copies for buffer views whose offset inside their
/// enclosing title buffer is not a multiple of their element stride, keyed by
/// `(start, len)`.
static ALIGNED: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// Returns a host copy holding `[address, address + len)` whose offset for
/// `address` is a multiple of `align` (required for structured/typed buffer
/// views whose `FirstElement` cannot express sub-element byte offsets).
/// Uses the enclosing buffer shadow on the fast path when already aligned.
pub(crate) fn find_aligned(address: u64, len: u64, align: u64) -> Option<Arc<Shadow>> {
    let shadow = find(address, len)?;
    if align <= 1 || shadow.offset(address).is_multiple_of(align) {
        return Some(shadow);
    }
    let end = address.checked_add(len.max(1))?;
    let mut map = ALIGNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map
        .range(..=(address, u64::MAX))
        .rev()
        .take(32)
        .find(|&(&(start, slen), _)| {
            start <= address
                && start.saturating_add(slen) >= end
                && (address - start).is_multiple_of(align)
        })
        .map(|(_, s)| s.clone())
    {
        return Some(existing);
    }
    let bytes = len.max(align).next_multiple_of(1 << 16);
    let (buffer, data) = host()?.shared_buffer(bytes)?;
    crate::host::name(&buffer, || {
        format!("aligned buffer {address:#x}+{bytes:#x} (align {align})")
    });
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{address:#x}"),
        len = bytes,
        align,
        "aligned sub-buffer shadow created"
    );
    let shadow = Arc::new(Shadow::new(
        (buffer, data, gpu, bytes),
        Kind::Aligned,
        address,
        bytes,
    ));
    map.insert((address, bytes), shadow.clone());
    Some(shadow)
}

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(
        (buffer, data, gpu, len),
        Kind::Loose,
        start,
        len,
    ));
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    all.extend(
        ALIGNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_runs_finds_exact_changed_byte_ranges() {
        let before = vec![0u8; 256];
        let mut after = before.clone();
        assert!(diff_runs(&before, &after).is_empty());

        after[3] = 0xAA;
        after[4] = 0xBB;
        after[130..135].fill(0x55);
        after[255] = 0xFF;
        assert_eq!(
            diff_runs(&before, &after),
            vec![(3, 5), (130, 135), (255, 256)]
        );
    }

    #[test]
    fn merge_ranges_coalesces_overlapping_and_adjacent_spans() {
        assert_eq!(
            merge_ranges(&[(100, 200), (50, 80), (80, 120), (300, 300), (250, 260)]),
            vec![(50, 200), (250, 260)]
        );
    }

    #[test]
    fn duplicate_gpu_writes_do_not_block_later_refreshes() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mut title_mem = vec![0x11u8; 4096];
        let base = title_mem.as_mut_ptr() as u64;
        let fake_id = (base as usize) ^ 0xD0D0_5A5A;
        register(fake_id, base, title_mem.len() as u64);

        let shadow = find(base, 4096).expect("shadow buffer");
        shadow.update(base as usize, 4096);
        let range = (base as usize, 4096);
        shadow.writing(range.0, range.1);
        shadow.writing(range.0, range.1);
        assert_eq!(shadow.pending.lock().unwrap().len(), 1);

        // Host::execute snapshots and copies back an exact-unique range once.
        let snapshot = shadow.snapshot_ranges(&[range]);
        assert_eq!(shadow.copy_back_ranges(&[range], snapshot.as_ref()), 0);
        assert!(shadow.pending.lock().unwrap().is_empty());

        title_mem[128..136].fill(0x99);
        shadow.update(range.0, range.1);
        // SAFETY: inside shadow's live mapping; update must not skip this range.
        let refreshed = unsafe { std::slice::from_raw_parts(shadow.data.add(128), 8) };
        assert_eq!(refreshed, &[0x99u8; 8]);

        unregister(fake_id, base);
        let _ = host_gpu;
    }

    /// A diff-only copy-back into a write-watched page preserves a concurrent
    /// title write elsewhere in that page; the page must not then count as
    /// synchronized, or the next refresh would skip the preserved bytes.
    #[test]
    fn copy_back_does_not_mark_watched_pages_fresh() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        assert!(
            base.is_multiple_of(PAGE),
            "allocation granularity is 64 KiB"
        );
        // SAFETY: the committed page just allocated, used only by this test.
        let title_mem = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, PAGE) };
        title_mem.fill(0x11);
        let fake_id = base ^ 0x5EED_F00D;
        register(fake_id, base as u64, PAGE as u64);

        let shadow = find(base as u64, PAGE as u64).expect("shadow buffer");
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(shadow.watched.load(Ordering::Relaxed), 1);
        shadow.writing(base, PAGE);
        let snapshot = shadow.snapshot_ranges(&[(base, PAGE)]);

        // The GPU writes [64, 72) of the copy while the title writes
        // [200, 208) of its memory, in the same 64 KiB page.
        // SAFETY: inside the shadow's mapping.
        unsafe { std::slice::from_raw_parts_mut(shadow.data.add(64), 8).fill(0x77) };
        title_mem[200..208].fill(0x99);

        assert_eq!(
            shadow.copy_back_ranges(&[(base, PAGE)], snapshot.as_ref()),
            8
        );
        assert_eq!(&title_mem[64..72], &[0x77u8; 8]);
        assert_eq!(&title_mem[200..208], &[0x99u8; 8]);

        // The next refresh brings the preserved title bytes into the copy.
        assert!(shadow.update(base, PAGE) > 0);
        assert_eq!(shadow.watched.load(Ordering::Relaxed), 1);
        // SAFETY: inside the shadow's mapping.
        let copy = unsafe { std::slice::from_raw_parts(shadow.data, PAGE) };
        assert_eq!(&copy[200..208], &[0x99u8; 8], "title write refreshed");
        assert_eq!(&copy[64..72], &[0x77u8; 8]);

        unregister(fake_id, base as u64);
        drop(shadow);
        // SAFETY: our allocation; the shadow no longer refers to it.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
        let _ = host_gpu;
    }

    #[test]
    fn copy_back_diff_preserves_concurrent_cpu_writes_to_untouched_bytes() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mut title_mem = vec![0x11u8; 4096];
        let base = title_mem.as_mut_ptr() as u64;
        let fake_id = (base as usize) ^ 0xCAFE_BABE;
        register(fake_id, base, title_mem.len() as u64);

        let shadow = find(base, 4096).expect("shadow buffer");
        // Seed shadow from title_mem (all 0x11).
        shadow.update(base as usize, 4096);
        shadow.writing(base as usize, 4096);
        let snapshot = shadow.snapshot_ranges(&[(base as usize, 4096)]);

        // Simulate GPU writing ONLY bytes [64..72) in the shadow, while the
        // title CPU concurrently writes bytes [200..208) in title_mem.
        // SAFETY: inside shadow's 4096-byte mapping.
        unsafe {
            std::slice::from_raw_parts_mut(shadow.data.add(64), 8).fill(0x77);
        }
        title_mem[200..208].fill(0x99);

        let changed = shadow.copy_back_ranges(
            &[(base as usize, 4096), (base as usize, 4096)],
            snapshot.as_ref(),
        );
        assert_eq!(changed, 8);
        assert_eq!(&title_mem[64..72], &[0x77u8; 8], "GPU writes copied back");
        assert_eq!(
            &title_mem[200..208],
            &[0x99u8; 8],
            "concurrent title CPU writes preserved"
        );
        assert_eq!(&title_mem[0..64], &[0x11u8; 64]);

        unregister(fake_id, base);
        let _ = host_gpu;
    }

    /// Shader writes abandoned without copy-back (an unconfirmed fence wait)
    /// stop blocking refreshes, and title memory is left as the title wrote it.
    #[test]
    fn released_gpu_writes_allow_refreshes_again() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mut title_mem = vec![0x11u8; 4096];
        let base = title_mem.as_mut_ptr() as usize;
        let fake_id = base ^ 0xAB4D_0123;
        register(fake_id, base as u64, 4096);
        let shadow = find(base as u64, 4096).expect("shadow buffer");
        shadow.update(base, 4096);
        shadow.writing(base, 4096);
        title_mem[8..16].fill(0x99);
        assert_eq!(shadow.update(base, 4096), 0, "blocked while pending");
        shadow.release(&[(base, 4096)]);
        assert!(shadow.pending.lock().unwrap().is_empty());
        assert_eq!(shadow.update(base, 4096), 4096);
        // SAFETY: inside the shadow's mapping.
        let copy = unsafe { std::slice::from_raw_parts(shadow.data.add(8), 8) };
        assert_eq!(copy, &[0x99u8; 8]);
        unregister(fake_id, base as u64);
        let _ = host_gpu;
    }

    /// Two submissions in flight on one shadow keep their own baselines:
    /// the later snapshot neither replaces nor consumes the earlier one.
    #[test]
    fn overlapping_submissions_keep_their_own_snapshots() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mut title_mem = vec![0x11u8; 4096];
        let base = title_mem.as_mut_ptr() as usize;
        let fake_id = base ^ 0x0B5E_55ED;
        register(fake_id, base as u64, 4096);
        let shadow = find(base as u64, 4096).expect("shadow buffer");
        shadow.update(base, 4096);
        let (first, second) = ((base, 1024), (base + 2048, 1024));
        shadow.writing(first.0, first.1);
        shadow.writing(second.0, second.1);

        let before_first = shadow.snapshot_ranges(&[first]).expect("first snapshot");
        // SAFETY: inside the shadow's mapping.
        let copy = unsafe { std::slice::from_raw_parts_mut(shadow.data, 4096) };
        copy[16..24].fill(0x77); // the first submission's GPU write
        let before_second = shadow.snapshot_ranges(&[second]).expect("second snapshot");
        copy[2048 + 32..2048 + 40].fill(0x55); // the second's

        // The title writes the first range meanwhile; only GPU-changed
        // bytes may overwrite it, which needs the first baseline intact.
        title_mem[512..520].fill(0x99);
        assert_eq!(shadow.copy_back_ranges(&[first], Some(&before_first)), 8);
        assert_eq!(shadow.copy_back_ranges(&[second], Some(&before_second)), 8);
        assert_eq!(&title_mem[16..24], &[0x77u8; 8]);
        assert_eq!(&title_mem[512..520], &[0x99u8; 8], "title write preserved");
        assert_eq!(&title_mem[2048 + 32..2048 + 40], &[0x55u8; 8]);
        assert!(shadow.pending.lock().unwrap().is_empty());

        unregister(fake_id, base as u64);
        let _ = host_gpu;
    }

    /// The pool never keeps more than its byte budget: a buffer bigger than
    /// the whole budget is freed instead of kept.
    #[test]
    fn snapshot_pool_keeps_within_its_byte_budget() {
        return_snapshot_buf(Vec::with_capacity(POOL_BYTES + 1));
        let pool = SNAPSHOT_POOL.lock().unwrap();
        assert!(pool.len() <= POOL_BUFFERS);
        assert!(pool.iter().map(Vec::capacity).sum::<usize>() <= POOL_BYTES);
        assert!(pool.iter().all(|b| b.capacity() <= POOL_BYTES));
    }

    /// An aligned copy is counted with its kind and size, and released (so
    /// uncounted by its `Drop`) once unregistering its buffer prunes it and
    /// the last holder lets it go. Other tests make aligned copies too, so
    /// the global counters are not compared exactly.
    #[test]
    fn aligned_shadows_are_counted_until_released() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let title_mem = vec![0u8; 4096];
        let base = title_mem.as_ptr() as u64;
        let fake_id = (base as usize) ^ 0xA11C_0DE5;
        register(fake_id, base, 4096);
        let shadow = find_aligned(base + 4, 256, 16).expect("aligned shadow");
        assert_eq!(shadow.kind, Kind::Aligned);
        assert_eq!(shadow.bytes, 1 << 16);
        assert!(memstat::live(Kind::Aligned).0 >= shadow.bytes);
        unregister(fake_id, base);
        let weak = Arc::downgrade(&shadow);
        drop(shadow);
        assert!(
            weak.upgrade().is_none(),
            "unregister pruned the aligned copy"
        );
        let _ = host_gpu;
    }
}
