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
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_MAPPED,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READWRITE,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQuery,
    VirtualQueryEx,
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
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
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
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        if info.host.State == MEM_RESERVE
            && info.host.Type == MEM_MAPPED
            && is_mapped_physical(address as usize)
        {
            info.host.State = MEM_COMMIT;
            info.host.Protect = PAGE_READWRITE;
        }
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
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(number) };
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
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
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
        let Some(slice) = pages
            .get(page)
            .and_then(|p| physical().pages.get(p).cloned())
        else {
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
        if newly_committed {
            let mut remapped = Vec::new();
            {
                let physical = physical();
                for (&view_base, &(view_len, ref vslice)) in &physical.views {
                    if Arc::ptr_eq(&vslice.section, &slice.section) {
                        let pages_in_view = view_len / PAGE;
                        if slice.index >= vslice.index && slice.index < vslice.index + pages_in_view
                        {
                            let addr = view_base + (slice.index - vslice.index) * PAGE;
                            let _ = commit(addr, PAGE);
                            remapped.push(addr);
                        }
                    }
                }
            }
            for addr in remapped {
                victory_gpu::title_memory_remapped(addr, PAGE);
            }
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
    let mut physical = physical();
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
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

/// Whether `address` currently lies inside a mapped physical-page view.
fn is_mapped_physical(address: usize) -> bool {
    let physical = physical();
    physical
        .views
        .range(..=address)
        .next_back()
        .is_some_and(|(&base, &(len, _))| address < base.saturating_add(len))
}

/// Ensures every demand-mapped physical page overlapping `[at, at + len)` is
/// committed in its view before a host kernel write (`ReadFile`, `fread`,
/// DirectStorage memory read) or GPU copy-back writes into it.
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
}

/// Registers the vectored exception handler that demand-commits `SEC_RESERVE`
/// physical-page views on first CPU read/write (64 KiB granularity) and wires
/// `victory_gpu::use_commit_range(ensure_committed)`.
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
        let at = r.ExceptionInformation[1];
        let committed_page = {
            let physical = physical();
            let Some((&view_base, &(_view_len, ref slice))) = physical
                .views
                .range(..=at)
                .next_back()
                .filter(|&(&base, &(size, _))| at < base.saturating_add(size))
            else {
                return EXCEPTION_CONTINUE_SEARCH;
            };
            let page_base = at & !(PAGE - 1);
            let page_idx = slice.index + (page_base - view_base) / PAGE;
            // SAFETY: plain `VirtualQuery` on our mapped view.
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid out pointer.
            let queried =
                unsafe { VirtualQuery(page_base as *const _, &mut mbi, size_of_val(&mbi)) };
            if queried == 0 {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            if mbi.State == MEM_COMMIT {
                // If another thread just demand-committed this page to RW while
                // we waited for `PHYSICAL`, retry the faulting instruction.
                // If it is `PAGE_READONLY`, let `victory_gpu::watch`'s VEH
                // handle the write-watch guard fault.
                if (mbi.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE)) != 0 {
                    return EXCEPTION_CONTINUE_EXECUTION;
                }
                return EXCEPTION_CONTINUE_SEARCH;
            }
            if mbi.State != MEM_RESERVE {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            if !commit(page_base, PAGE) {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            slice.section.committed(page_idx, 1);
            Some(page_base)
        };
        if let Some(page_base) = committed_page {
            victory_gpu::title_memory_remapped(page_base, PAGE);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        victory_gpu::use_commit_range(ensure_committed);
        // SAFETY: handler only commits pages in our own `PHYSICAL.views`.
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
        // the view so reads see the written byte immediately.
        assert!(write_physical(&pages[3..4], 4, &[0x77]));
        // SAFETY: mapped physical view.
        assert_eq!(unsafe { base.cast::<u8>().add(3 * PAGE + 4).read() }, 0x77);

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
}
