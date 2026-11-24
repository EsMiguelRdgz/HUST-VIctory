//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_DECOMMIT, MEM_EXTENDED_PARAMETER,
    MEM_MAPPED, MEM_PRESERVE_PLACEHOLDER, MEM_PRIVATE, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, MemExtendedParameterAddressRequirements, PAGE_EXECUTE_READ,
    PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, SEC_RESERVE,
    UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQuery, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (Grand Theft
/// Auto VI's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

/// Minimum `XMemVirtualAlloc` commit size eligible for 64 KiB demand commitment.
/// Smaller allocations (stacks, scratch buffers, small structs) stay eagerly
/// committed to avoid VEH overhead and host syscall edge cases.
const DEMAND_PRIVATE_MIN_BYTES: usize = 4 * MIB;
const OS_PAGE: usize = 4096;

/// Opt-out diagnostic toggle (`VICTORY_EAGER_PRIVATE_COMMIT=1`) to commit large
/// `XMemVirtualAlloc` private pools eagerly instead of demand-committing each
/// 64 KiB chunk on first CPU/kernel/GPU access.
fn eager_private_commit() -> bool {
    static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EAGER
        .get_or_init(|| std::env::var_os("VICTORY_EAGER_PRIVATE_COMMIT").is_some_and(|v| v == "1"))
}

/// Coalesced set of disjoint `[start, end)` half-open intervals representing
/// logically committed `MEM_PRIVATE` `XMemVirtualAlloc` ranges whose backing
/// pages are demand-committed 64 KiB at a time on first access.
#[derive(Default)]
struct DemandPrivate {
    ranges: BTreeMap<usize, usize>,
}

impl DemandPrivate {
    const fn new() -> Self {
        Self {
            ranges: BTreeMap::new(),
        }
    }

    fn add(&mut self, mut start: usize, mut end: usize) {
        if start >= end {
            return;
        }
        if let Some((&prev_start, &prev_end)) = self.ranges.range(..=start).next_back()
            && prev_end >= start
        {
            start = prev_start;
            end = end.max(prev_end);
            self.ranges.remove(&prev_start);
        }
        let overlapping: Vec<(usize, usize)> = self
            .ranges
            .range(start..=end)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in overlapping {
            self.ranges.remove(&s);
            end = end.max(e);
        }
        self.ranges.insert(start, end);
    }

    fn remove(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let first = self
            .ranges
            .range(..=start)
            .next_back()
            .filter(|&(_, &e)| e > start)
            .map(|(&s, &e)| (s, e));
        let hit: Vec<(usize, usize)> = first
            .into_iter()
            .chain(
                self.ranges
                    .range(start.saturating_add(1)..end)
                    .map(|(&s, &e)| (s, e)),
            )
            .collect();
        for (s, e) in hit {
            self.ranges.remove(&s);
            if s < start {
                self.ranges.insert(s, start);
            }
            if e > end {
                self.ranges.insert(end, e);
            }
        }
    }

    fn contains(&self, addr: usize) -> Option<(usize, usize)> {
        self.ranges
            .range(..=addr)
            .next_back()
            .filter(|&(_, &e)| addr < e)
            .map(|(&s, &e)| (s, e))
    }

    fn overlaps(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        if start >= end {
            return Vec::new();
        }
        let first = self
            .ranges
            .range(..=start)
            .next_back()
            .filter(|&(_, &e)| e > start)
            .map(|(&s, &e)| (s, e));
        first
            .into_iter()
            .chain(
                self.ranges
                    .range(start.saturating_add(1)..end)
                    .map(|(&s, &e)| (s, e)),
            )
            .map(|(s, e)| (s.max(start), e.min(end)))
            .filter(|&(s, e)| s < e)
            .collect()
    }
}

static DEMAND_PRIVATE: Mutex<DemandPrivate> = Mutex::new(DemandPrivate::new());

fn demand_private() -> MutexGuard<'static, DemandPrivate> {
    DEMAND_PRIVATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Checks whether `[lo, hi)` is entirely backed by an existing `MEM_PRIVATE`
/// reservation (`MEM_RESERVE` or `MEM_COMMIT`), with no `MEM_FREE` holes or
/// non-private mappings.
fn all_private_reserved_or_committed(lo: usize, hi: usize) -> bool {
    let mut cur = lo;
    while cur < hi {
        // SAFETY: plain `VirtualQuery` output struct.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid output pointer of the right size.
        let n = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
        if n == 0
            || info.RegionSize == 0
            || info.Type != MEM_PRIVATE
            || (info.State != MEM_RESERVE && info.State != MEM_COMMIT)
        {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= cur {
            return false;
        }
        cur = next;
    }
    true
}

/// Returns `[alloc_base, end)` for the virtual memory reservation containing `address`.
fn reservation_extent(address: usize) -> Option<(usize, usize)> {
    // SAFETY: plain `VirtualQuery` output struct.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid output pointer.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 || info.RegionSize == 0 || info.AllocationBase.is_null() {
        return None;
    }
    let alloc_base = info.AllocationBase as usize;
    let mut cur = alloc_base;
    loop {
        // SAFETY: plain `VirtualQuery` output struct.
        let mut sub: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid output pointer.
        let q = unsafe { VirtualQuery(cur as *const _, &mut sub, size_of_val(&sub)) };
        if q == 0 || sub.RegionSize == 0 || sub.AllocationBase as usize != alloc_base {
            break;
        }
        let next = (sub.BaseAddress as usize).saturating_add(sub.RegionSize);
        if next <= cur {
            break;
        }
        cur = next;
    }
    (cur > alloc_base).then_some((alloc_base, cur))
}

/// Scrubs demand-private tracking before a title `VirtualFree` releases or
/// decommits memory so stale ranges can never outlive their reservation.
pub(crate) fn on_virtual_free(address: *mut c_void, size: usize, free_type: u32) {
    if address.is_null() {
        return;
    }
    let addr = address as usize;
    if (free_type & MEM_RELEASE) != 0 {
        if let Some((lo, hi)) = reservation_extent(addr) {
            demand_private().remove(lo, hi);
        }
        return;
    }
    if (free_type & MEM_DECOMMIT) != 0 {
        if size == 0 {
            if let Some((lo, hi)) = reservation_extent(addr) {
                demand_private().remove(lo, hi);
            }
        } else {
            let lo = addr & !(OS_PAGE - 1);
            let hi = addr
                .saturating_add(size)
                .checked_next_multiple_of(OS_PAGE)
                .unwrap_or(usize::MAX);
            demand_private().remove(lo, hi);
        }
    }
}

/// Commits any untouched `MEM_RESERVE | MEM_PRIVATE` pages inside tracked
/// demand-private ranges overlapping `[at, at + len)`.
pub(crate) fn ensure_private_committed(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let dp = demand_private();
    let overlaps = dp.overlaps(at, end);
    if overlaps.is_empty() {
        return;
    }
    for (ov_lo, ov_hi) in overlaps {
        let lo = ov_lo & !(OS_PAGE - 1);
        let hi = ov_hi.checked_next_multiple_of(OS_PAGE).unwrap_or(ov_hi);
        let mut cur = lo;
        while cur < hi {
            // SAFETY: plain `VirtualQuery` output struct.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid output pointer.
            let n = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
            if n == 0 || info.RegionSize == 0 {
                break;
            }
            let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize);
            let chunk_end = region_end.min(hi);
            if info.State == MEM_RESERVE && info.Type == MEM_PRIVATE && chunk_end > cur {
                let _ = commit(cur, chunk_end - cur);
            }
            if region_end <= cur {
                break;
            }
            cur = region_end;
        }
    }
}

/// Adjusts a host `MEMORY_BASIC_INFORMATION` for `XMemVirtualQuery` /
/// `VirtualQuery` / `VirtualQueryEx` so logically committed demand-mapped
/// physical views and demand-committed private pools report `MEM_COMMIT`
/// and `PAGE_READWRITE`.
pub(crate) fn adjust_query_info(address: usize, info: &mut MEMORY_BASIC_INFORMATION) {
    if info.State != MEM_RESERVE {
        return;
    }
    let base = info.BaseAddress as usize;
    if info.Type == MEM_MAPPED {
        let physical = physical();
        if let Some((&view_base, &(view_len, _))) = physical
            .views
            .range(..=address)
            .next_back()
            .filter(|&(&b, &(len, _))| address < b.saturating_add(len))
        {
            info.State = MEM_COMMIT;
            info.Protect = PAGE_READWRITE;
            let view_end = view_base.saturating_add(view_len);
            if view_end > base {
                info.RegionSize = info.RegionSize.min(view_end - base);
            }
        }
        return;
    }
    if info.Type == MEM_PRIVATE
        && let Some((_, range_end)) = demand_private().contains(address)
    {
        info.State = MEM_COMMIT;
        info.Protect = PAGE_READWRITE;
        if range_end > base {
            info.RegionSize = info.RegionSize.min(range_end - base);
        }
    }
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (Grand Theft Auto VI's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let demand_commit_private = !eager_private_commit()
        && !placeholder
        && (kind & MEM_COMMIT) != 0
        && protect == PAGE_READWRITE
        && size >= DEMAND_PRIVATE_MIN_BYTES;

    // Committing a large sub-range of an existing private reservation: keep
    // pages `MEM_RESERVE` until first CPU/kernel/GPU access instead of eagerly
    // charging gigabytes of unused zero pages to system commit.
    if !base.is_null() && (kind & MEM_RESERVE) == 0 && demand_commit_private {
        let lo = (base as usize) & !(OS_PAGE - 1);
        if let Some(hi) = (base as usize)
            .checked_add(size)
            .and_then(|e| e.checked_next_multiple_of(OS_PAGE))
            && hi > lo
            && all_private_reserved_or_committed(lo, hi)
        {
            install_demand_commit();
            demand_private().add(lo, hi);
            tracing::debug!(
                base = format_args!("{base:p}"),
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                "XMemVirtualAlloc sub-commit demand-private"
            );
            return lo as *mut c_void;
        }
    }

    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    let alloc_kind = if constrained && demand_commit_private {
        kind & !MEM_COMMIT
    } else {
        kind
    };
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            alloc_kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if !at.is_null() && constrained {
        let lo = at as usize;
        let hi = lo.saturating_add(size.next_multiple_of(OS_PAGE));
        if demand_commit_private {
            install_demand_commit();
            let mut dp = demand_private();
            dp.remove(lo, hi);
            dp.add(lo, hi);
        } else if size > 0 {
            demand_private().remove(lo, hi);
        }
    }
    if base.is_null() || size >= DEMAND_PRIVATE_MIN_BYTES {
        tracing::debug!(
            base = format_args!("{base:p}"),
            size,
            kind = format_args!("{kind:#x}"),
            alloc_kind = format_args!("{alloc_kind:#x}"),
            flags = format_args!("{flags:#x}"),
            protect = format_args!("{protect:#x}"),
            at = format_args!("{at:p}"),
            demand_private = constrained && demand_commit_private,
            "XMemVirtualAlloc"
        );
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (Grand Theft Auto VI's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    if !out.is_null() {
        ensure_committed(out as usize, size_of::<BasicInformation>());
    }
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        adjust_query_info(address as usize, &mut info.host);
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (Grand Theft Auto VI does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (Grand Theft Auto VI uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here an allocation is
// pagefile-backed sections of up to `CHUNK` pages each, its pages are 64 KiB
// slices of them, and a mapping is a view of consecutive slices of one
// section in place of a host placeholder.
//
// A section's committed pages are charged to the system commit until the
// section is gone, which is when every page of it has been freed and
// unmapped: pages of one section cannot be decommitted on their own. One
// section per allocation (Grand Theft Auto VI asks for 6.1 GiB at once) kept
// a whole pool committed while any page of it was still allocated; chunks
// give back what the title frees, 16 MiB at a time.

const PAGE: usize = 64 << 10;

/// Pages per section.
const CHUNK: usize = 256;

struct Section {
    handle: HANDLE,
    /// Which of its pages have been committed (pages are committed when
    /// first mapped or written, and stay so for the section's life).
    committed: Mutex<Vec<bool>>,
}

impl Section {
    /// Notes pages `[first, first + n)` committed; counts those that were not
    /// and returns how many pages became newly committed.
    fn committed(&self, first: usize, n: usize) -> usize {
        let mut committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pages) = committed.get_mut(first..first + n) else {
            return 0;
        };
        let new = pages.iter().filter(|&&c| !c).count();
        if new > 0 {
            pages.fill(true);
            victory_gpu::memstat::adjust(
                victory_gpu::memstat::Kind::GuestPhysical,
                (new * PAGE) as u64,
                0,
            );
        }
        new
    }

    /// Returns contiguous `(offset_in_range, count)` runs within `[first, first + n)`
    /// that are already committed in this section.
    fn committed_runs(&self, first: usize, n: usize) -> Vec<(usize, usize)> {
        let committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pages) = committed.get(first..first + n) else {
            return Vec::new();
        };
        let mut runs = Vec::new();
        let mut i = 0;
        while i < pages.len() {
            if !pages[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < pages.len() && pages[i] {
                i += 1;
            }
            runs.push((start, i - start));
        }
        runs
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        let committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = committed.iter().filter(|&&c| c).count() * PAGE;
        victory_gpu::memstat::free(victory_gpu::memstat::Kind::GuestPhysical, bytes as u64);
        // SAFETY: our handle; views keep the section alive on their own,
        // and none is left: every slice (views hold theirs) is gone.
        unsafe { CloseHandle(self.handle) };
    }
}

/// A pagefile-backed section of `pages` pages, none committed yet.
fn section(pages: usize) -> Option<Section> {
    let size = (pages * PAGE) as u64;
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE | SEC_RESERVE,
            (size >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        return None;
    }
    victory_gpu::memstat::alloc(victory_gpu::memstat::Kind::GuestPhysical, 0);
    Some(Section {
        handle,
        committed: Mutex::new(vec![false; pages]),
    })
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Reverse index from `section.handle as usize` to `(view_base, view_len, slice_index)`
    /// for O(1) active-view lookup during `write_physical` (avoids O(views) scan).
    section_views: HashMap<usize, Vec<(usize, usize, usize)>>,
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    if n.checked_mul(PAGE).is_none_or(|s| s == 0) {
        return 0;
    }
    // Sections of up to `CHUNK` pages, committed page by page when first
    // mapped or written (`commit`): Grand Theft Auto VI asks for 6.1 GiB at
    // once, which a busy host often cannot commit (error 1450).
    let mut sections = Vec::with_capacity(n.div_ceil(CHUNK));
    for first in (0..n).step_by(CHUNK) {
        let Some(section) = section(CHUNK.min(n - first)) else {
            let error = std::io::Error::last_os_error();
            tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
            victory_gpu::memstat::count(victory_gpu::memstat::Event::Failures, 1);
            // All or nothing: the sections made so far close as they drop.
            // SAFETY: checked non-null.
            unsafe { count.write(0) };
            return 0;
        };
        sections.push(Arc::new(section));
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let first = {
        let mut physical = physical();
        let first = physical.next_page;
        physical.next_page += n * FRAMES;
        for index in 0..n {
            let slice = Slice {
                section: sections[index / CHUNK].clone(),
                index: index % CHUNK,
            };
            let number = first + index * FRAMES;
            physical.pages.insert(number, slice);
        }
        first
    };
    // Write the title's output array AFTER releasing `PHYSICAL`: if `pages`
    // lies in a demand-committed private pool, writing it may fault into
    // `on_physical_fault`, which must never re-enter `PHYSICAL` on this thread.
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: Grand Theft Auto VI keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // Copy title memory BEFORE acquiring `PHYSICAL` so demand-commit faults
    // cannot re-enter `PHYSICAL` on this thread.
    // SAFETY: the title passes `n` page numbers.
    let list: Vec<usize> = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut physical = physical();
    for page in list {
        physical.pages.remove(&page);
    }
    1
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile};
    let mut done = 0;
    while done < data.len() {
        let (page, within) = ((offset + done) / PAGE, (offset + done) % PAGE);
        // Read the title's page number BEFORE acquiring `PHYSICAL` so a
        // demand-commit fault on `pages` cannot re-enter `PHYSICAL`.
        let Some(&page_num) = pages.get(page) else {
            return false;
        };
        let Some(slice) = physical().pages.get(&page_num).cloned() else {
            return false;
        };
        let at = (slice.index * PAGE) as u64;
        // SAFETY: one page of a section we own, mapped for the copy only.
        let view = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (at >> 32) as u32,
                at as u32,
                PAGE,
            )
        };
        if view.Value.is_null() {
            return false;
        }
        if !commit(view.Value as usize, PAGE) {
            // SAFETY: our view.
            unsafe { UnmapViewOfFile(view) };
            return false;
        }
        let newly_committed = slice.section.committed(slice.index, 1) > 0;
        let n = (PAGE - within).min(data.len() - done);
        // SAFETY: the view holds `PAGE` bytes; `within + n <= PAGE`.
        unsafe {
            view.Value
                .cast::<u8>()
                .add(within)
                .copy_from_nonoverlapping(data[done..].as_ptr(), n);
            UnmapViewOfFile(view);
        }
        let mut remapped = Vec::new();
        {
            let physical = physical();
            if let Some(views) = physical.section_views.get(&(slice.section.handle as usize)) {
                for &(view_base, view_len, view_slice_index) in views {
                    let pages_in_view = view_len / PAGE;
                    if slice.index >= view_slice_index
                        && slice.index < view_slice_index + pages_in_view
                    {
                        let addr = view_base + (slice.index - view_slice_index) * PAGE;
                        if newly_committed {
                            let _ = commit(addr, PAGE);
                        }
                        remapped.push(addr);
                    }
                }
            }
        }
        for addr in remapped {
            victory_gpu::title_memory_remapped(addr, PAGE);
        }
        done += n;
    }
    true
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    // Copy the title's page-number array BEFORE acquiring `PHYSICAL` so a
    // demand-commit fault on `pages` cannot re-enter `PHYSICAL` on this thread.
    let page_list = if pages.is_null() {
        None
    } else {
        // SAFETY: the title passes `n` page numbers.
        Some(unsafe { std::slice::from_raw_parts(pages, n) }.to_vec())
    };
    let mut physical = physical();
    unmap(&mut physical, base, n * PAGE);
    let Some(pages) = page_list else {
        return address;
    };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (Grand Theft Auto VI passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`): Grand Theft Auto
/// VI maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut physical = physical();
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// Opt-out diagnostic toggle (`VICTORY_EAGER_PHYSICAL_COMMIT=1`) to commit all
/// physical pages eagerly on `XMemMapPhysicalPages` instead of demand-committing
/// each 64 KiB page on first CPU/kernel/GPU access.
fn eager_physical_commit() -> bool {
    static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EAGER
        .get_or_init(|| std::env::var_os("VICTORY_EAGER_PHYSICAL_COMMIT").is_some_and(|v| v == "1"))
}

/// Ensures every demand-mapped physical page and demand-committed private page
/// overlapping `[at, at + len)` is committed before a host kernel read/write
/// (`ReadFile`, `WriteFile`, `fread`, DirectStorage) or GPU copy-back touches it.
pub fn ensure_committed(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let mut remapped = Vec::new();
    {
        let physical = physical();
        let first = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|&(&base, &(size, _))| base.saturating_add(size) > at)
            .map(|(&base, _)| base);
        let view_bases: Vec<usize> = first
            .into_iter()
            .chain(
                physical
                    .views
                    .range(at.saturating_add(1)..end)
                    .map(|(&b, _)| b),
            )
            .collect();
        for view_base in view_bases {
            let Some(&(view_len, ref slice)) = physical.views.get(&view_base) else {
                continue;
            };
            let lo = at.max(view_base) & !(PAGE - 1);
            let hi = end
                .min(view_base.saturating_add(view_len))
                .next_multiple_of(PAGE);
            let hi = hi.min(view_base.saturating_add(view_len));
            if hi <= lo {
                continue;
            }
            let first_page = slice.index + (lo - view_base) / PAGE;
            let page_count = (hi - lo) / PAGE;
            // Only call `VirtualAlloc(MEM_COMMIT)` if any sub-range is still
            // uncommitted (`MEM_RESERVE`), avoiding redundant syscalls on
            // already-committed hot ranges.
            let mut cur = lo;
            let mut needs_commit = false;
            while cur < hi {
                // SAFETY: plain `VirtualQuery` on a mapped view.
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: valid output pointer of the right size.
                let n = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
                if n == 0 || info.RegionSize == 0 {
                    needs_commit = true;
                    break;
                }
                if info.State != MEM_COMMIT {
                    needs_commit = true;
                    break;
                }
                let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
                if next <= cur {
                    break;
                }
                cur = next;
            }
            if needs_commit && commit(lo, hi - lo) {
                slice.section.committed(first_page, page_count);
                remapped.push((lo, hi - lo));
            }
        }
    }
    for (base, size) in remapped {
        victory_gpu::title_memory_remapped(base, size);
    }
    ensure_private_committed(at, len);
}

/// Registers the vectored exception handler that demand-commits `SEC_RESERVE`
/// physical-page views and large `XMemVirtualAlloc` private pools on first CPU
/// read/write (64 KiB granularity) and wires `victory_gpu::use_commit_range`.
fn install_demand_commit() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };

    unsafe extern "system" fn on_physical_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the OS passes a valid `EXCEPTION_POINTERS` record.
        let r = unsafe { &*(*info).ExceptionRecord };
        const READ: usize = 0;
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || (r.ExceptionInformation[0] != READ && r.ExceptionInformation[0] != WRITE)
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let access = r.ExceptionInformation[0];
        let at = r.ExceptionInformation[1];
        enum PhysicalOutcome {
            NotInViews,
            Committed(usize),
            ContinueExecution,
            ContinueSearch,
        }
        let outcome = {
            let physical = physical();
            if let Some((&view_base, &(_view_len, ref slice))) = physical
                .views
                .range(..=at)
                .next_back()
                .filter(|&(&base, &(size, _))| at < base.saturating_add(size))
            {
                let page_base = at & !(PAGE - 1);
                let page_idx = slice.index + (page_base - view_base) / PAGE;
                // SAFETY: plain `VirtualQuery` on our mapped view.
                let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: valid out pointer.
                let queried =
                    unsafe { VirtualQuery(page_base as *const _, &mut mbi, size_of_val(&mbi)) };
                if queried == 0 {
                    PhysicalOutcome::ContinueSearch
                } else if mbi.State == MEM_COMMIT {
                    // Another thread may have demand-committed this page while we
                    // waited for `PHYSICAL` (and `watch::guard()` may even have
                    // immediately transitioned it to `PAGE_READONLY`).
                    // - For a READ fault, any readable protection means the read
                    //   can now succeed (`watch::on_write` only handles writes, so
                    //   returning CONTINUE_SEARCH on a read of PAGE_READONLY would
                    //   erroneously crash).
                    // - For a WRITE fault, only writable protections retry directly;
                    //   `PAGE_READONLY` falls through to `watch::on_write`.
                    let writable = (mbi.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE)) != 0;
                    let readable =
                        writable || (mbi.Protect & (PAGE_READONLY | PAGE_EXECUTE_READ)) != 0;
                    if (access == READ && readable) || writable {
                        PhysicalOutcome::ContinueExecution
                    } else {
                        PhysicalOutcome::ContinueSearch
                    }
                } else if mbi.State == MEM_RESERVE && commit(page_base, PAGE) {
                    slice.section.committed(page_idx, 1);
                    PhysicalOutcome::Committed(page_base)
                } else {
                    PhysicalOutcome::ContinueSearch
                }
            } else {
                PhysicalOutcome::NotInViews
            }
        };
        match outcome {
            PhysicalOutcome::Committed(page_base) => {
                victory_gpu::title_memory_remapped(page_base, PAGE);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            PhysicalOutcome::ContinueExecution => return EXCEPTION_CONTINUE_EXECUTION,
            PhysicalOutcome::ContinueSearch => return EXCEPTION_CONTINUE_SEARCH,
            PhysicalOutcome::NotInViews => {}
        }

        // Check demand-committed `XMemVirtualAlloc` private pools.
        let dp = demand_private();
        let Some((range_start, range_end)) = dp.contains(at) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        let os_page = at & !(OS_PAGE - 1);
        // SAFETY: plain `VirtualQuery` on our tracked private range.
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointer.
        let queried = unsafe { VirtualQuery(os_page as *const _, &mut mbi, size_of_val(&mbi)) };
        if queried == 0 || mbi.RegionSize == 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        if mbi.State == MEM_COMMIT {
            let writable = (mbi.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE)) != 0;
            let readable = writable || (mbi.Protect & (PAGE_READONLY | PAGE_EXECUTE_READ)) != 0;
            if (access == READ && readable) || writable {
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            return EXCEPTION_CONTINUE_SEARCH;
        }
        if mbi.State != MEM_RESERVE || mbi.Type != MEM_PRIVATE {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let page_lo = at & !(PAGE - 1);
        let page_hi = page_lo.saturating_add(PAGE);
        let mbi_lo = mbi.BaseAddress as usize;
        let mbi_hi = mbi_lo.saturating_add(mbi.RegionSize);
        let commit_lo = page_lo.max(range_start).max(mbi_lo);
        let commit_hi = page_hi.min(range_end).min(mbi_hi);
        if commit_hi > commit_lo && commit(commit_lo, commit_hi - commit_lo) {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        victory_gpu::use_commit_range(ensure_committed);
        // SAFETY: handler only commits pages in our own tracked views/pools.
        unsafe { AddVectoredExceptionHandler(1, Some(on_physical_fault)) };
    });
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    install_demand_commit();
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if eager_physical_commit() {
        if !commit(base, len) {
            // SAFETY: the view just mapped, back to a placeholder.
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
            return false;
        }
        slice.section.committed(slice.index, len / PAGE);
    } else {
        // Demand-commit mode: untouched pages remain `MEM_RESERVE` (0 bytes of
        // system commit) until first CPU, kernel, or GPU write access. Any
        // pages already committed in this section (e.g. written via
        // `write_physical` or aliased via `map_alias` / `unmap` split) are
        // made `MEM_COMMIT` in this view at 0 additional system commit cost.
        for (off_pages, count_pages) in slice.section.committed_runs(slice.index, len / PAGE) {
            if !commit(base + off_pages * PAGE, count_pages * PAGE) {
                // SAFETY: the view just mapped, back to a placeholder.
                unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
                return false;
            }
        }
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    physical
        .section_views
        .entry(slice.section.handle as usize)
        .or_default()
        .push((base, len, slice.index));
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    let done =
        !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null();
    if !done {
        tracing::warn!(
            len,
            error = %std::io::Error::last_os_error(),
            "committing physical pages failed"
        );
        victory_gpu::memstat::count(victory_gpu::memstat::Event::Failures, 1);
    }
    done
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        let sec_key = slice.section.handle as usize;
        if let Some(entries) = physical.section_views.get_mut(&sec_key) {
            entries.retain(|&(b, _, _)| b != at);
            if entries.is_empty() {
                physical.section_views.remove(&sec_key);
            }
        }
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// Synthetic benchmark (run alone, `--ignored --test-threads=1`): system
    /// commit charged by 512 MiB of physical pages at each step of their
    /// life. Prints `phase commit-delta-MiB` lines; the regression test
    /// [`partly_freed_physical_pages_return_their_commit`] asserts on them.
    #[test]
    #[ignore]
    fn bench_physical_page_commit() {
        let steps: Vec<String> = physical_page_commit_steps(512 * MIB)
            .into_iter()
            .map(|(phase, delta)| format!("{phase}={:+.0}MiB", delta as f64 / MIB as f64))
            .collect();
        let line = format!("physical-pages 512MiB commit-delta {}", steps.join(" "));
        println!("{line}");
        victory_gpu::memstat::report(&format!("synthetic {line}"));
    }

    /// Freeing most of a pool gives its commit back (it used to stay charged
    /// until the last page was freed), and freeing all of it gives back the
    /// rest. Under demand-commit, mapping alone charges ~0 MiB until pages
    /// are actually touched. The system commit moves with other processes too:
    /// margins.
    #[test]
    fn partly_freed_physical_pages_return_their_commit() {
        let steps = physical_page_commit_steps(256 * MIB);
        let step = |name| steps.iter().find(|s| s.0 == name).expect("step").1 / MIB as i128;
        assert!(step("allocated") < 64, "{steps:?}");
        if !eager_physical_commit() {
            assert!(
                step("mapped") < 64,
                "untouched mapped physical pool must not charge system commit: {steps:?}"
            );
        }
        assert!(step("touched") > 192, "{steps:?}");
        assert!(
            step("unmapped") > 192,
            "allocated pages keep their data: {steps:?}"
        );
        assert!(step("seven-eighths-freed") < 128, "{steps:?}");
        assert!(step("all-freed") < 64, "{steps:?}");
    }

    /// Pages committed through views are counted once per page, and
    /// uncounted when their sections go.
    #[test]
    fn physical_page_commit_is_accounted() {
        use victory_gpu::memstat::{Kind, live};
        let mut pages = [0usize; 2 * CHUNK];
        let mut n = pages.len();
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let at = XMemVirtualAlloc(
            std::ptr::null_mut(),
            n * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!at.is_null());
        let before = live(Kind::GuestPhysical).0;
        assert_eq!(XMemMapPhysicalPages(at, n, pages.as_ptr()), at);
        if !eager_physical_commit() {
            // Untouched mapped pages consume 0 GuestPhysical commit.
            assert!(
                live(Kind::GuestPhysical).0 < before + (n * PAGE) as u64,
                "demand-mapped pages should not be eagerly committed"
            );
        }
        // Touch every page via CPU write so all `n` pages are committed.
        for i in 0..n {
            // SAFETY: inside our mapped view; VEH demand-commits each page.
            unsafe { at.cast::<u8>().add(i * PAGE).write(1) };
        }
        // Mapped twice (a second view of the first chunk): still counted once.
        assert!(write_physical(&pages[..1], 0, &[1]));
        let mapped = live(Kind::GuestPhysical).0;
        assert!(mapped >= before + (n * PAGE) as u64, "{before} {mapped}");
        assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 1);
        // Other tests commit physical pages too: what this one freed is gone.
        assert!(live(Kind::GuestPhysical).0 + (n * PAGE) as u64 <= mapped + (64 * PAGE) as u64);
        // SAFETY: our own placeholder reservation.
        unsafe { VirtualFree(at, 0, MEM_RELEASE) };
    }

    #[test]
    fn demand_committed_physical_pages_commit_on_read_write_and_kernel_io() {
        use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
        let mut pages = [0usize; 6];
        let mut n = pages.len();
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let base = XMemVirtualAlloc(
            std::ptr::null_mut(),
            n * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!base.is_null());
        assert_eq!(XMemMapPhysicalPages(base, n, pages.as_ptr()), base);

        // XMemVirtualQuery reports mapped physical views as committed graphics memory.
        let qi = query(base);
        assert_eq!(qi.host.State, MEM_COMMIT);
        assert_eq!(qi.flags, GRAPHICS);

        // Page 0: first CPU read demand-commits zeroed page.
        // SAFETY: mapped physical view.
        assert_eq!(unsafe { base.cast::<u8>().read() }, 0);

        // Page 1: first CPU write demand-commits and persists.
        // SAFETY: mapped physical view.
        unsafe { base.cast::<u8>().add(PAGE).write(0x5A) };
        // SAFETY: mapped physical view.
        assert_eq!(unsafe { base.cast::<u8>().add(PAGE).read() }, 0x5A);

        // Page 2: untouched page becomes kernel-writable via `ensure_committed`.
        let p2 = base.wrapping_byte_add(2 * PAGE);
        ensure_committed(p2 as usize, PAGE);
        let payload = [0xA5u8; 16];
        let mut written = 0usize;
        // SAFETY: `p2` was committed by `ensure_committed`.
        let ok = unsafe {
            WriteProcessMemory(
                GetCurrentProcess(),
                p2,
                payload.as_ptr().cast(),
                payload.len(),
                &mut written,
            )
        };
        assert_ne!(ok, 0);
        assert_eq!(written, payload.len());
        // SAFETY: `p2` is committed and contains `payload`.
        assert_eq!(unsafe { p2.cast::<u8>().read() }, 0xA5);

        // Page 3: `write_physical` into an untouched already-mapped view commits
        // the view so reads see the written byte immediately; a subsequent
        // overwrite of the ALREADY-committed page also propagates immediately.
        assert!(write_physical(&pages[3..4], 4, &[0x77]));
        // SAFETY: mapped physical view.
        assert_eq!(unsafe { base.cast::<u8>().add(3 * PAGE + 4).read() }, 0x77);
        assert!(write_physical(&pages[3..4], 4, &[0x88]));
        // SAFETY: mapped physical view.
        assert_eq!(unsafe { base.cast::<u8>().add(3 * PAGE + 4).read() }, 0x88);

        // Pages 4..6: concurrent multi-threaded reads and writes on untouched pages.
        let p4 = base.wrapping_byte_add(4 * PAGE) as usize;
        let p5 = base.wrapping_byte_add(5 * PAGE) as usize;
        std::thread::scope(|s| {
            for t in 0..8usize {
                s.spawn(move || {
                    // SAFETY: mapped physical views; VEH serializes concurrent faults.
                    unsafe {
                        let _ = (p4 as *const u8).add(t).read();
                        (p5 as *mut u8).add(t).write((t + 1) as u8);
                    }
                });
            }
        });
        for t in 0..8usize {
            // SAFETY: written above.
            assert_eq!(unsafe { (p5 as *const u8).add(t).read() }, (t + 1) as u8);
        }

        assert_eq!(XMemMapPhysicalPages(base, n, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 1);
        // SAFETY: our own placeholder reservation.
        unsafe { VirtualFree(base, 0, MEM_RELEASE) };
    }

    #[test]
    fn concurrent_demand_commit_reads_and_readonly_protection_never_fault_out() {
        use windows_sys::Win32::System::Memory::VirtualProtect;
        const N: usize = 8;
        let mut pages = [0usize; N];
        let mut n = N;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let base = XMemVirtualAlloc(
            std::ptr::null_mut(),
            N * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!base.is_null());
        assert_eq!(XMemMapPhysicalPages(base, N, pages.as_ptr()), base);

        let base_addr = base as usize;
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            // Protector thread repeatedly simulates `watch::guard()` marking
            // committed pages `PAGE_READONLY` and `title_memory_remapped()`
            // restoring `PAGE_READWRITE`.
            s.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    for i in 0..N {
                        let page = base_addr + i * PAGE;
                        let mut old = 0u32;
                        // SAFETY: inside our mapped view; toggling RO/RW only.
                        let ok = unsafe {
                            VirtualProtect(page as *const _, PAGE, PAGE_READONLY, &mut old)
                        };
                        if ok != 0 {
                            // SAFETY: restoring RW on our own view.
                            unsafe {
                                VirtualProtect(page as *const _, PAGE, PAGE_READWRITE, &mut old)
                            };
                        }
                    }
                }
            });
            // Reader threads concurrently fault on untouched pages while the
            // protector races to apply `PAGE_READONLY`.
            for t in 0..4usize {
                s.spawn(move || {
                    for iter in 0..32usize {
                        for i in 0..N {
                            let p = (base_addr + i * PAGE + t * 8 + (iter % 8)) as *const u8;
                            // SAFETY: inside our demand-mapped view.
                            let _ = unsafe { p.read_volatile() };
                        }
                    }
                });
            }
            // Wait for readers to finish before stopping the protector thread.
            std::thread::sleep(std::time::Duration::from_millis(20));
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        assert_eq!(XMemMapPhysicalPages(base, N, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
        // SAFETY: our own placeholder reservation.
        unsafe { VirtualFree(base, 0, MEM_RELEASE) };
    }

    /// Allocates `size` bytes of physical pages, maps them, touches them,
    /// unmaps them, frees the first 7/8, then the rest; the system commit after
    /// each step relative to before the allocation.
    fn physical_page_commit_steps(size: usize) -> Vec<(&'static str, i128)> {
        let commit = || victory_gpu::memstat::system_commit().expect("commit").0 as i128;
        let n = size / PAGE;
        let mut pages = vec![0usize; n];
        let base = commit();
        let mut steps = Vec::new();
        let mut count = n;
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
            1
        );
        steps.push(("allocated", commit() - base));
        let at = XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!at.is_null());
        assert_eq!(XMemMapPhysicalPages(at, n, pages.as_ptr()), at);
        steps.push(("mapped", commit() - base));
        for i in 0..n {
            // SAFETY: inside our mapped view; demand-commits every 64 KiB page.
            unsafe { at.cast::<u8>().add(i * PAGE).write(1) };
        }
        steps.push(("touched", commit() - base));
        assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        steps.push(("unmapped", commit() - base));
        let freed = n / 8 * 7;
        assert_eq!(XMemFreePhysicalPages(freed, pages.as_ptr()), 1);
        steps.push(("seven-eighths-freed", commit() - base));
        assert_eq!(XMemFreePhysicalPages(n - freed, pages[freed..].as_ptr()), 1);
        steps.push(("all-freed", commit() - base));
        // SAFETY: our own placeholder reservation.
        unsafe { VirtualFree(at, 0, MEM_RELEASE) };
        steps
    }

    #[test]
    fn demand_private_interval_set_coalescing_and_splitting() {
        let mut set = DemandPrivate::new();
        set.add(0x1000, 0x3000);
        set.add(0x3000, 0x5000);
        assert_eq!(set.contains(0x2500), Some((0x1000, 0x5000)));
        set.add(0x8000, 0xA000);
        set.add(0x4000, 0x8000);
        assert_eq!(set.contains(0x7000), Some((0x1000, 0xA000)));
        set.remove(0x3000, 0x6000);
        assert_eq!(set.contains(0x2000), Some((0x1000, 0x3000)));
        assert_eq!(set.contains(0x4000), None);
        assert_eq!(set.contains(0x7000), Some((0x6000, 0xA000)));
        assert_eq!(
            set.overlaps(0x2000, 0x8000),
            vec![(0x2000, 0x3000), (0x6000, 0x8000)]
        );
    }

    #[test]
    fn demand_private_pool_commits_only_touched_pages_and_preserves_write_watch() {
        let size = 64 * MIB;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS,
            PAGE_READWRITE,
        );
        assert!(!p.is_null());

        // Logical query reports committed graphics RW memory while OS pages
        // remain uncommitted MEM_RESERVE (0 commit charge until touched).
        let xq = query(p);
        assert_eq!(xq.host.State, MEM_COMMIT);
        assert_eq!(xq.host.Protect, PAGE_READWRITE);
        assert_eq!(xq.flags & GRAPHICS, GRAPHICS);

        // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain zeroable C struct.
        let mut hle_mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            crate::modules::kernel::VirtualQuery(
                p,
                (&raw mut hle_mbi).cast(),
                size_of::<MEMORY_BASIC_INFORMATION>(),
            ),
            0
        );
        assert_eq!(hle_mbi.State, MEM_COMMIT);
        assert_eq!(hle_mbi.Protect, PAGE_READWRITE);

        if !eager_private_commit() {
            // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain zeroable C struct.
            let mut raw: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid out pointer on our allocation.
            assert_ne!(unsafe { VirtualQuery(p, &mut raw, size_of_val(&raw)) }, 0);
            assert_eq!(
                raw.State, MEM_RESERVE,
                "untouched pool must stay MEM_RESERVE"
            );
        }

        // Write-watch still works natively on partially committed regions.
        let mut found = [0usize; 64];
        let mut count = found.len();
        let mut gran = 0u32;
        assert_eq!(
            // SAFETY: room for `count` entries.
            unsafe {
                windows_sys::Win32::System::Memory::GetWriteWatch(
                    1,
                    p,
                    size,
                    found.as_mut_ptr().cast(),
                    &mut count,
                    &mut gran,
                )
            },
            0
        );
        assert_eq!(count, 0);

        // First read demand-commits page 0 as zero; write demand-commits page 10.
        // SAFETY: inside our 64 MiB logically-committed pool.
        unsafe {
            assert_eq!(p.cast::<u8>().read(), 0);
            p.cast::<u8>().add(10 * PAGE).write(0x5A);
            assert_eq!(p.cast::<u8>().add(10 * PAGE).read(), 0x5A);
        }

        // Untouched page 100 remains MEM_RESERVE.
        if !eager_private_commit() {
            // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain zeroable C struct.
            let mut raw100: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let p100 = p.wrapping_byte_add(100 * PAGE);
            // SAFETY: valid out pointer inside our allocation.
            let q = unsafe { VirtualQuery(p100, &mut raw100, size_of_val(&raw100)) };
            assert_ne!(q, 0);
            assert_eq!(raw100.State, MEM_RESERVE);
        }

        // GetWriteWatch reports the written page (page 10).
        count = found.len();
        assert_eq!(
            // SAFETY: room for `count` entries.
            unsafe {
                windows_sys::Win32::System::Memory::GetWriteWatch(
                    1,
                    p,
                    size,
                    found.as_mut_ptr().cast(),
                    &mut count,
                    &mut gran,
                )
            },
            0
        );
        let p10_addr = p as usize + 10 * PAGE;
        assert!(
            found[..count].contains(&p10_addr),
            "written page 10 missing from GetWriteWatch: {:?}",
            &found[..count]
        );

        // HLE VirtualProtect on an untouched page commits it first and succeeds.
        let p200 = p.wrapping_byte_add(200 * PAGE);
        let mut old = 0u32;
        assert_ne!(
            crate::modules::kernel::VirtualProtect(p200, PAGE, PAGE_READONLY, &mut old),
            0
        );
        // SAFETY: page 200 is committed read-only zero memory.
        assert_eq!(unsafe { p200.cast::<u8>().read() }, 0);

        // HLE VirtualFree(MEM_DECOMMIT) clears demand-private tracking for that slice.
        let p300 = p.wrapping_byte_add(300 * PAGE);
        assert_ne!(
            crate::modules::kernel::VirtualFree(p300, PAGE, MEM_DECOMMIT),
            0
        );
        assert_eq!(query(p300).host.State, MEM_RESERVE);

        // Sub-range commit on an existing MEM_RESERVE pool.
        let reserved = XMemVirtualAlloc(
            std::ptr::null_mut(),
            32 * MIB,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!reserved.is_null());
        let sub = reserved.wrapping_byte_add(8 * MIB);
        let committed = XMemVirtualAlloc(sub, 8 * MIB, MEM_COMMIT, GRAPHICS, PAGE_READWRITE);
        assert_eq!(committed, sub);
        assert_eq!(query(sub).host.State, MEM_COMMIT);
        // SAFETY: logically committed sub-range.
        unsafe {
            sub.cast::<u8>().add(3 * PAGE).write(0xC3);
            assert_eq!(sub.cast::<u8>().add(3 * PAGE).read(), 0xC3);
        }

        // HLE VirtualFree(MEM_RELEASE) cleans up both allocations.
        assert_ne!(crate::modules::kernel::VirtualFree(p, 0, MEM_RELEASE), 0);
        assert_ne!(
            crate::modules::kernel::VirtualFree(reserved, 0, MEM_RELEASE),
            0
        );
        assert!(demand_private().contains(p as usize).is_none());
        assert!(demand_private().contains(sub as usize).is_none());
    }

    #[test]
    fn demand_private_concurrent_faults_and_kernel_ensure_committed() {
        let size = 16 * MIB;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS,
            PAGE_READWRITE,
        );
        assert!(!p.is_null());
        let base = p as usize;
        let threads: Vec<_> = (0..4usize)
            .map(|tid| {
                std::thread::spawn(move || {
                    for i in 0..64usize {
                        let page = base + i * PAGE;
                        if (i + tid) % 5 == 0 {
                            ensure_committed(page, PAGE);
                        }
                        // SAFETY: disjoint byte per thread within each 64 KiB page.
                        unsafe {
                            let slot = (page as *mut u8).add(tid * 16);
                            let _ = slot.read_volatile();
                            slot.write_volatile((tid as u8).wrapping_add(i as u8));
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("demand-private worker panicked");
        }
        for i in 0..64usize {
            let page = base + i * PAGE;
            for tid in 0..4usize {
                // SAFETY: all written pages are committed.
                let got = unsafe { (page as *const u8).add(tid * 16).read_volatile() };
                assert_eq!(got, (tid as u8).wrapping_add(i as u8));
            }
        }
        assert_ne!(crate::modules::kernel::VirtualFree(p, 0, MEM_RELEASE), 0);
    }

    /// Regression test for the Test #8 boot deadlock: when the title places its
    /// `pages` array inside an untouched demand-committed private pool and passes
    /// it to `XMemAllocatePhysicalPages`, `XMemMapPhysicalPages`, `write_physical`,
    /// or `XMemFreePhysicalPages`, faulting in `pages` must never re-enter `PHYSICAL`.
    #[test]
    fn allocate_free_and_map_physical_pages_using_untouched_demand_private_arrays() {
        let pool = XMemVirtualAlloc(
            std::ptr::null_mut(),
            8 * MIB,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS,
            PAGE_READWRITE,
        );
        assert!(!pool.is_null());
        // Place the page-number array on untouched pages well inside the pool.
        let pages_ptr = pool.wrapping_byte_add(2 * MIB).cast::<usize>();
        let mut n = 4096usize; // 32 KiB of page numbers spanning untouched pages
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages_ptr), 1);

        let view = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!view.is_null());
        assert_eq!(XMemMapPhysicalPages(view, 4, pages_ptr), view);
        // SAFETY: `pages_ptr` holds `n >= 4` page numbers.
        let slice = unsafe { std::slice::from_raw_parts(pages_ptr, 4) };
        assert!(write_physical(slice, 0, &[0xAB; 64]));
        // SAFETY: page 0 is mapped at `view`.
        assert_eq!(unsafe { view.cast::<u8>().read() }, 0xAB);

        assert_eq!(XMemMapPhysicalPages(view, 4, std::ptr::null()), view);
        assert_eq!(XMemFreePhysicalPages(n, pages_ptr), 1);
        assert_ne!(crate::modules::kernel::VirtualFree(view, 0, MEM_RELEASE), 0);
        assert_ne!(crate::modules::kernel::VirtualFree(pool, 0, MEM_RELEASE), 0);
    }
}
