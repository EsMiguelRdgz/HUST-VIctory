//! Heaps, committed and placed resources, and resource sizes.
//!
//! Heaps and resources are host memory and a resource's GPU address is its
//! CPU address, as on the console; textures also get a host copy, and
//! buffers a host copy when a draw reads them (`shadow`).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::NonNull;

use victory_formats::texture;

use crate::object::{
    self, E_INVALIDARG, E_OUTOFMEMORY, HEAP, HRESULT, Iid, Object, RESOURCE, S_OK, Table, This,
    guid,
};

/// The interfaces every device child also answers for.
pub(crate) const PAGEABLE_IIDS: [Iid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];
const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x6b3b2502,
        0x6e51,
        0x45b3,
        [0x90, 0xee, 0x98, 0x84, 0x26, 0x5e, 0x8d, 0xf3],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];
const RESOURCE_IIDS: [Iid; 4] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const KIB_64: u64 = 64 << 10;
const MIB_4: u64 = 4 << 20;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    size: u64,
    properties: HeapProperties,
    alignment: u64,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    size: u64,
    alignment: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
const _: () = assert!(size_of::<ResourceDesc>() == 56);

const BUFFER: u32 = 1;
const TEXTURE_3D: u32 = 4;

/// Bytes per element and the element's width in texels (4 for block
/// compression), by DXGI format number. `None` for 1-bit and video
/// formats. The console adds 116/117 (10-bit float color), 118-120 (16-bit
/// depth with 8-bit stencil, two planes: 3 bytes per texel), 189 (signed
/// 10-bit color), 190 (two 4-bit channels) and 191 (24/8 color).
pub(crate) fn element(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

/// A texture's layout in console memory, as the console's layout library
/// gives it (`victory-hle`'s `xg` serves the same): the description's
/// layout field is `0x100 | swizzle mode`, 1 linear, 0 the library's pick.
pub(crate) fn texture_layout(desc: &ResourceDesc) -> Option<texture::Texture> {
    use texture::{Dimension, Swizzle, TextureDesc};
    const TEXTURE_1D: u32 = 2;
    const TEXTURE_2D: u32 = 3;
    const RENDER_TARGET: u32 = 1;
    const DEPTH: u32 = 2;
    let dimension = match desc.dimension {
        TEXTURE_1D => Dimension::Tex1d,
        TEXTURE_2D => Dimension::Tex2d,
        TEXTURE_3D => Dimension::Tex3d,
        _ => return None,
    };
    let swizzle = match desc.layout {
        0 if dimension == Dimension::Tex1d => Swizzle::LINEAR,
        0 if desc.flags & DEPTH != 0 => Swizzle::SW_64KB_Z_X,
        0 if desc.flags & RENDER_TARGET != 0 => Swizzle::SW_64KB_R_X,
        0 => Swizzle::SW_4KB_S,
        1 => Swizzle::LINEAR,
        l if l & !0xff == 0x100 => Swizzle(l & 0xff),
        _ => return None,
    };
    texture::Texture::new(&TextureDesc {
        dimension,
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array_size),
        mips: u32::from(desc.mip_levels),
        format: desc.format,
        samples: desc.sample_count.max(1),
        swizzle,
    })
    .ok()
}

/// Bytes and alignment a resource takes: buffers their width (64 KiB
/// aligned), textures their console layout (formats it does not know:
/// every mip of every array slice and sample, packed, 64 KiB aligned),
/// rounded up to the alignment.
fn footprint(desc: &ResourceDesc) -> Option<AllocationInfo> {
    if desc.dimension != BUFFER
        && let Some(t) = texture_layout(desc)
    {
        // As the console: the layout's own alignment unless more is asked
        // (4 KiB for 4 KiB modes, where PC says 64 KiB). Grand Theft Auto
        // VI keeps these sizes in its texture tables, and its streaming
        // asserts when they disagree with the streamed layout.
        let alignment = desc.alignment.max(t.alignment);
        if !alignment.is_power_of_two() {
            return None;
        }
        return Some(AllocationInfo {
            size: t.size.checked_next_multiple_of(alignment)?,
            alignment,
        });
    }
    let bytes = if desc.dimension == BUFFER {
        desc.width
    } else {
        let (element, block) = element(desc.format)?;
        let (depth, slices) = if desc.dimension == TEXTURE_3D {
            (u64::from(desc.depth_or_array_size), 1)
        } else {
            (1, u64::from(desc.depth_or_array_size))
        };
        let height = u64::from(desc.height);
        let mips = mip_count(desc);
        let mut chain = 0u64;
        for mip in 0..mips.min(64) {
            let w = (desc.width >> mip).max(1).div_ceil(block);
            let h = (height >> mip).max(1).div_ceil(block);
            let d = (depth >> mip).max(1);
            chain = chain.checked_add(w.checked_mul(h)?.checked_mul(d)?.checked_mul(element)?)?;
        }
        chain
            .checked_mul(slices)?
            .checked_mul(u64::from(desc.sample_count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let minimum = if desc.sample_count > 1 { MIB_4 } else { KIB_64 };
    let alignment = desc.alignment.max(minimum);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        size: bytes.checked_next_multiple_of(KIB_64)?,
        alignment,
    })
}

/// A format as the host has it. The console's 16-bit depth with stencil
/// (and its typeless views) has no PC format: the 24-bit ones stand in.
pub(crate) fn pc_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

/// The typeless format of a format's family (itself if it has none).
pub(crate) fn family(format: u32) -> u32 {
    const FAMILIES: [(u32, u32); 22] = [
        (1, 4),
        (5, 8),
        (9, 14),
        (15, 18),
        (19, 22),
        (23, 25),
        (27, 32),
        (33, 38),
        (39, 43),
        (44, 47),
        (48, 52),
        (53, 59),
        (60, 65),
        (70, 72),
        (73, 75),
        (76, 78),
        (79, 81),
        (82, 84),
        (90, 91),
        (92, 93),
        (94, 96),
        (97, 99),
    ];
    match format {
        87 => 90,
        88 => 92,
        f => FAMILIES
            .iter()
            .find(|&&(first, last)| (first..=last).contains(&f))
            .map_or(f, |&(first, _)| first),
    }
}

/// sRGB formats and their linear twins.
pub(crate) const SRGB: [(u32, u32); 3] = [(29, 28), (91, 87), (93, 88)];

/// The format a view of a title texture must name when its host copy is
/// typeless (see [`DEPTH_FORMATS`], [`SRGB`]): the title's own; `None` if
/// the host copy is typed. Computed from the resource description without
/// forcing lazy host-texture allocation.
pub(crate) fn view_format(this: This) -> Option<u32> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let own = pc_format(r.desc.format);
    let host = crate::host::host()?;
    let format = host_texture_desc(host, &r.desc).format;
    (format != own).then_some(own)
}

/// If `this` is a texture whose unsupported console color format (16-bit packed
/// color or `R9G9B9E5_SHAREDEXP` RT/UAV) was promoted to a host-supported DXGI
/// format, returns that host format. Computed without forcing lazy host-texture
/// allocation.
#[cfg(windows)]
pub(crate) fn promoted_format(this: This) -> Option<u32> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let host = crate::host::host()?;
    let title = pc_format(r.desc.format);
    if !is_packed_16bit_color(title) && !is_shared_exp_color(title) {
        return None;
    }
    let d = host_texture_desc(host, &r.desc);
    (d.format != title).then_some(d.format)
}

/// Depth formats with their typeless family: depth textures that shaders
/// read are made typeless on the host, which only allows views of
/// typeless depth resources.
pub(crate) const DEPTH_FORMATS: [(u32, u32); 4] = [(20, 19), (40, 39), (45, 44), (55, 53)];

/// Mip levels, the full chain for 0.
fn mip_count(desc: &ResourceDesc) -> u32 {
    let depth = if desc.dimension == TEXTURE_3D {
        u64::from(desc.depth_or_array_size)
    } else {
        1
    };
    match desc.mip_levels {
        0 => {
            64 - desc
                .width
                .max(u64::from(desc.height))
                .max(depth)
                .max(1)
                .leading_zeros()
        }
        n => u32::from(n),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFootprint {
    pub(crate) offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    row_pitch: u32,
}

impl PlacedFootprint {
    /// The bytes it spans in a buffer: every row pitched but the last.
    pub(crate) fn bytes(&self) -> Option<u64> {
        let (element, block) = element(self.format)?;
        let lines = u64::from(self.height).div_ceil(block) * u64::from(self.depth);
        let last = u64::from(self.width).div_ceil(block) * element;
        Some(u64::from(self.row_pitch) * lines.saturating_sub(1) + last)
    }
}
const _: () = assert!(size_of::<PlacedFootprint>() == 32);

const NO_FOOTPRINT: PlacedFootprint = PlacedFootprint {
    offset: u64::MAX,
    format: 0,
    width: 0,
    height: 0,
    depth: 0,
    row_pitch: 0,
};

/// One subresource's size in an upload buffer: footprint (offset 0), rows
/// and bytes per row. Planes of depth-stencil formats are not told apart.
fn subresource(desc: &ResourceDesc, index: u32) -> Option<(PlacedFootprint, u32, u64)> {
    const PITCH: u64 = 256;
    if desc.dimension == BUFFER {
        let footprint = PlacedFootprint {
            offset: 0,
            format: 0,
            width: u32::try_from(desc.width).ok()?,
            height: 1,
            depth: 1,
            row_pitch: u32::try_from(desc.width.next_multiple_of(PITCH)).ok()?,
        };
        return (index == 0).then_some((footprint, 1, desc.width));
    }
    let mips = mip_count(desc);
    let slices = if desc.dimension == TEXTURE_3D {
        1
    } else {
        u32::from(desc.depth_or_array_size).max(1)
    };
    let (mip, slice) = (index % mips, index / mips);
    if slice >= slices {
        return None;
    }
    let (element, block) = element(desc.format)?;
    let width = (desc.width >> mip).max(1).next_multiple_of(block);
    let height = (u64::from(desc.height) >> mip)
        .max(1)
        .next_multiple_of(block);
    let depth = if desc.dimension == TEXTURE_3D {
        (u32::from(desc.depth_or_array_size) >> mip).max(1)
    } else {
        1
    };
    let row_size = width / block * element;
    let footprint = PlacedFootprint {
        offset: 0,
        format: desc.format,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        depth,
        row_pitch: u32::try_from(row_size.next_multiple_of(PITCH)).ok()?,
    };
    Some((footprint, u32::try_from(height / block).ok()?, row_size))
}

/// Device method: where each subresource goes in an upload buffer, rows
/// 256-byte and subresources 512-byte aligned, as on PC. Unknown sizes give
/// the documented all-ones values.
pub(crate) extern "system" fn copyable_footprints(
    _: This,
    desc: *const ResourceDesc,
    first: u32,
    count: u32,
    base: u64,
    layouts: *mut PlacedFootprint,
    rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    const PLACEMENT: u64 = 512;
    // SAFETY: the title passes a description (null is tolerated).
    let desc = unsafe { desc.as_ref() };
    let mut end = base;
    let mut known = desc.is_some();
    for i in 0..count {
        let one = desc.and_then(|d| subresource(d, first + i));
        let (layout, row_count, row_size) = match one {
            Some((mut footprint, row_count, row_size)) => {
                footprint.offset = end.next_multiple_of(PLACEMENT);
                let pitch = u64::from(footprint.row_pitch);
                let lines = u64::from(row_count) * u64::from(footprint.depth);
                end = footprint.offset + pitch * lines.saturating_sub(1) + row_size;
                (footprint, row_count, row_size)
            }
            None => {
                known = false;
                (NO_FOOTPRINT, u32::MAX, u64::MAX)
            }
        };
        let i = i as usize;
        // SAFETY: each output is null or holds `count` entries.
        unsafe {
            if !layouts.is_null() {
                layouts.add(i).write(layout);
            }
            if !rows.is_null() {
                rows.add(i).write(row_count);
            }
            if !row_sizes.is_null() {
                row_sizes.add(i).write(row_size);
            }
        }
    }
    if !known {
        tracing::warn!(?desc, "no footprint for this resource");
    }
    if !total.is_null() {
        // SAFETY: checked non-null.
        unsafe { total.write(if known { end - base } else { u64::MAX }) };
    }
}

/// Device method: the footprint of resources placed one after another.
/// Returns a 16-byte value, through a hidden first argument. Invalid input
/// gives the documented size `u64::MAX`.
pub(crate) extern "system" fn allocation_info(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(place(descs.iter(), |_, _| ())) };
    out
}

/// The newer description: the plain one, then a sampler feedback mip
/// region (width, height, depth).
#[repr(C)]
pub(crate) struct ResourceDesc1 {
    desc: ResourceDesc,
    mip_region: [u32; 3],
}
const _: () = assert!(size_of::<ResourceDesc1>() == 72);

/// Where one resource goes in [`allocation_info2`]'s placement.
#[repr(C)]
pub(crate) struct AllocationInfo1 {
    offset: u64,
    alignment: u64,
    size: u64,
}

/// Device method 0x360 (`ID3D12Device8`): [`allocation_info`] for newer
/// descriptions, also telling where each resource goes.
pub(crate) extern "system" fn allocation_info2(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc1,
    each: *mut AllocationInfo1,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    let total = place(descs.iter().map(|d| &d.desc), |i, info| {
        if !each.is_null() {
            // SAFETY: the title passes room for `count` entries.
            unsafe { each.add(i).write(info) };
        }
    });
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(total) };
    out
}

/// Resources placed one after another: the whole footprint, telling `each`
/// where every resource goes. Unknown sizes give the documented `u64::MAX`.
fn place<'a>(
    descs: impl Iterator<Item = &'a ResourceDesc>,
    mut each: impl FnMut(usize, AllocationInfo1),
) -> AllocationInfo {
    let mut total = AllocationInfo {
        size: 0,
        alignment: KIB_64,
    };
    for (i, desc) in descs.enumerate() {
        let Some(one) = footprint(desc) else {
            tracing::warn!(?desc, "no size for this resource");
            each(
                i,
                AllocationInfo1 {
                    offset: u64::MAX,
                    alignment: 0,
                    size: u64::MAX,
                },
            );
            return AllocationInfo {
                size: u64::MAX,
                alignment: 0,
            };
        };
        tracing::debug!(?desc, ?one, "allocation info");
        let offset = total.size.next_multiple_of(one.alignment);
        each(
            i,
            AllocationInfo1 {
                offset,
                alignment: one.alignment,
                size: one.size,
            },
        );
        total.size = offset + one.size;
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

/// Zeroed host memory standing in for GPU memory.
pub(crate) struct Memory {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Set when the memory came from the title's allocator, with these
    /// allocation attributes (it goes back there).
    attributes: Option<u64>,
}

pub type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
pub type FreeRoutine = extern "system" fn(*mut c_void, u64);
static ALLOCATOR: std::sync::OnceLock<(AllocRoutine, FreeRoutine)> = std::sync::OnceLock::new();

/// Committed resources get their memory from the title's general
/// allocator, as on the console, where titles hook it to place graphics
/// memory themselves (Grand Theft Auto VI places its back buffers in a
/// block of its own and checks they fill it). The runtime names it before
/// the device exists.
pub fn use_allocator(alloc: AllocRoutine, free: FreeRoutine) {
    let _ = ALLOCATOR.set((alloc, free));
}

/// Maps the physical pages behind one range of title memory at another,
/// `(to, from, len)` (`from` 0 unmaps); false on failure.
pub type PageMapper = fn(u64, u64, u64) -> bool;
static PAGE_MAPPER: std::sync::OnceLock<PageMapper> = std::sync::OnceLock::new();

/// The runtime's page mapper, for the queues' page-mapping copies (memory
/// is the runtime's, as on the console).
pub fn use_page_mapper(mapper: PageMapper) {
    let _ = PAGE_MAPPER.set(mapper);
}

pub(crate) fn page_mapper() -> Option<PageMapper> {
    PAGE_MAPPER.get().copied()
}

/// Allocation attributes for resource memory: allocator id, page size
/// (bits 14-15: 2 MiB pages from 2 MiB up, else 64 KiB), alignment as log2
/// (bits 24-28) and memory type (bits 29-32: write-combined for GPU-only
/// heaps, cacheable for the CPU-visible ones).
/// shortcut: the console runtime's own choice of page size and memory type
/// is not known; this is the plausible one.
fn resource_attributes(info: AllocationInfo, heap_kind: u32) -> u64 {
    const GRAPHICS_ALLOCATOR: u64 = 128 << 16;
    const DEFAULT_HEAP: u32 = 1;
    let pages: u64 = if info.size >= 2 << 20 { 2 } else { 1 };
    let memory: u64 = if heap_kind == DEFAULT_HEAP { 1 } else { 3 };
    let alignment = u64::from(info.alignment.max(1).trailing_zeros());
    GRAPHICS_ALLOCATOR | pages << 14 | alignment << 24 | memory << 29
}

impl Memory {
    /// A committed resource's memory: from the title's allocator if the
    /// runtime named one, else our own; zeroed either way.
    fn for_resource(info: AllocationInfo, heap_kind: u32) -> Result<Memory, HRESULT> {
        let Some(&(alloc, _)) = ALLOCATOR.get() else {
            return Memory::new(info);
        };
        let layout = Layout::from_size_align(info.size as usize, info.alignment as usize)
            .map_err(|_| E_INVALIDARG)?;
        let attributes = resource_attributes(info, heap_kind);
        let ptr =
            NonNull::new(alloc(layout.size(), attributes).cast::<u8>()).ok_or(E_OUTOFMEMORY)?;
        // SAFETY: the allocator handed out `size` bytes.
        unsafe { ptr.as_ptr().write_bytes(0, layout.size()) };
        Ok(Memory {
            ptr,
            layout,
            attributes: Some(attributes),
        })
    }

    /// `size` zeroed bytes aligned to `alignment` (a power of two).
    pub(crate) fn zeroed(size: u64, alignment: u64) -> Result<Memory, HRESULT> {
        Memory::new(AllocationInfo { size, alignment })
    }

    pub(crate) fn address(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    fn new(info: AllocationInfo) -> Result<Memory, HRESULT> {
        let layout = usize::try_from(info.size)
            .ok()
            .zip(usize::try_from(info.alignment).ok())
            .filter(|&(size, _)| size > 0)
            .and_then(|(size, align)| Layout::from_size_align(size, align).ok())
            .ok_or(E_INVALIDARG)?;
        // SAFETY: the size is non-zero.
        let Some(ptr) = NonNull::new(unsafe { alloc_zeroed(layout) }) else {
            crate::memstat::count(crate::memstat::Event::Failures, 1);
            tracing::warn!(
                size = layout.size(),
                "no memory for a title heap or resource"
            );
            return Err(E_OUTOFMEMORY);
        };
        crate::memstat::alloc(crate::memstat::Kind::TitleHeap, layout.size() as u64);
        Ok(Memory {
            ptr,
            layout,
            attributes: None,
        })
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        match (self.attributes, ALLOCATOR.get()) {
            (Some(attributes), Some(&(_, free))) => free(self.ptr.as_ptr().cast(), attributes),
            _ => {
                crate::memstat::free(crate::memstat::Kind::TitleHeap, self.layout.size() as u64);
                // SAFETY: allocated in `new` with this layout.
                unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
            }
        }
    }
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: HeapDesc,
    memory: Memory,
}

pub(crate) extern "system" fn create_heap(
    _: This,
    desc: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(
        size = desc.size,
        kind = desc.properties.kind,
        flags = format_args!("{:#x}", desc.flags),
        "heap"
    );
    if desc.alignment != 0 && !desc.alignment.is_power_of_two() {
        return E_INVALIDARG;
    }
    let info = AllocationInfo {
        size: desc.size,
        alignment: desc.alignment.max(KIB_64),
    };
    let memory = match Memory::new(info) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    let heap = Box::new(Heap {
        object: Object::new(&HEAP_FUNCTIONS),
        desc,
        memory,
    });
    object::hand_out(HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn heap_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(HEAP, iid, object::is(iid, &HEAP_IIDS).then_some(this), out)
}
extern "system" fn heap_release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn heap_desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: `this` is a heap; the title passes room for a description.
        unsafe { out.write((*this.cast::<Heap>()).desc) };
    }
}

/// A resource's memory: its own, or a range of a heap it holds a reference
/// to.
enum Backing {
    Committed(Memory),
    Placed {
        heap: This,
        offset: u64,
    },
    /// Memory the title mapped itself, at this address.
    Address(*mut u8),
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Placed { heap, .. } = *self {
            heap_release(heap);
        }
    }
}

/// A depth target's unordered-access twin texture (PC forbids combining
/// `ALLOW_DEPTH_STENCIL` and `ALLOW_UNORDERED_ACCESS` on one resource, so UAV
/// views target this typeless twin and command lists copy between them at
/// state transitions).
#[cfg(windows)]
pub(crate) struct HostTwin {
    pub(crate) texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    pub(crate) state: std::sync::atomic::AtomicU32,
}

#[cfg(windows)]
#[derive(Default)]
struct HostTexSlot {
    resource: Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>,
    twin: Option<std::sync::Arc<HostTwin>>,
    failed: bool,
    bytes: u64,
}

#[cfg(windows)]
static TEXTURE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Whether host textures are created lazily on first GPU use (default: true).
/// Set `VICTORY_LAZY_TEXTURES=0` to restore eager creation for A/B comparison.
#[cfg(windows)]
pub(crate) fn lazy_textures_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_TEXTURES").is_none_or(|v| v != "0"))
}

#[repr(C)]
struct Resource {
    object: Object,
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    /// Textures also live on the host GPU (decision 0007); created lazily on
    /// first GPU use so dormant/unrendered title textures hold 0 commit.
    #[cfg(windows)]
    host: std::sync::Mutex<HostTexSlot>,
    /// Fast atomic mirror of `host.resource.is_some()`.
    #[cfg(windows)]
    host_live: std::sync::atomic::AtomicBool,
    /// Initial console resource state (used when lazily materializing `host`).
    #[cfg(windows)]
    initial_state: u32,
    /// Clear value copied at creation time (title pointer lives only for create).
    #[cfg(windows)]
    clear_value: Option<[u32; 5]>,
    /// Estimated footprint bytes currently counted under `Kind::DeferredTexture`.
    #[cfg(windows)]
    deferred_bytes: std::sync::atomic::AtomicU64,
    /// The state the host texture is in. The title's barriers name console
    /// states (looser than the host's), so we transition from this one.
    #[cfg(windows)]
    host_state: std::sync::atomic::AtomicU32,
    /// Monotonic last-use tick for LRU eviction of idle title-written textures.
    #[cfg(windows)]
    last_used: std::sync::atomic::AtomicU64,
    /// Active-list guard: cloned into `HostList.states` / `Step::Texture` while
    /// a list references this resource. `Arc::strong_count == 1` proves no open
    /// or in-flight command list references it (without altering COM refcount).
    #[cfg(windows)]
    list_guard: std::sync::Arc<()>,
}

/// Active-list guard for `this`, held by `HostList` until list reset/drop.
#[cfg(windows)]
pub(crate) fn list_guard(this: This) -> Option<std::sync::Arc<()>> {
    if this.is_null() {
        return None;
    }
    Some(resource(this).list_guard.clone())
}

/// Moves the host texture to `wanted` (a host texture state); the host
/// resource and the state it leaves, if that is a change.
#[cfg(windows)]
pub(crate) fn host_transition(this: This, wanted: u32) -> Option<(*mut c_void, u32)> {
    use windows::core::Interface;
    let host = existing_host_resource(this)?;
    let r = resource(this);
    let before = r
        .host_state
        .swap(wanted, std::sync::atomic::Ordering::Relaxed);
    (before != wanted).then(|| (host.as_raw(), before))
}

/// The host state a title resource's host copy is in now.
#[cfg(windows)]
pub(crate) fn host_state(this: This) -> u32 {
    resource(this)
        .host_state
        .load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(windows)]
pub(crate) fn swap_host_state(this: This, state: u32) -> u32 {
    resource(this)
        .host_state
        .swap(state, std::sync::atomic::Ordering::Relaxed)
}

/// Whether `this` currently has a materialized host D3D12 texture (lock-free).
#[cfg(windows)]
pub(crate) fn has_host_resource(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    resource(this)
        .host_live
        .load(std::sync::atomic::Ordering::Acquire)
}

/// Whether `this` is a title texture whose host D3D12 texture has not been
/// materialized yet.
#[cfg(windows)]
pub(crate) fn is_deferred_texture(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    let r = resource(this);
    r.desc.dimension != BUFFER && !r.host_live.load(std::sync::atomic::Ordering::Acquire)
}

/// Returns the host D3D12 texture for `this` ONLY if it is already
/// materialized, without triggering lazy allocation (for barriers and
/// diagnostics).
#[cfg(windows)]
pub(crate) fn existing_host_resource(
    this: This,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let slot = r.host.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(res) = &slot.resource {
        r.last_used.store(
            TEXTURE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        return Some(res.clone());
    }
    None
}

/// Returns `true` if `this` is a title-written texture (`Flags & 0x7 == 0`)
/// whose backing title memory has committed, readable pages containing at
/// least one non-zero byte. Used by bindless (`ResourceDescriptorHeap[]`)
/// draw/dispatch preparation so populated textures are materialized & uploaded
/// while uncommitted or all-zero placeholders stay deferred at 0 VRAM/commit.
#[cfg(windows)]
pub(crate) fn has_populated_title_data(this: This) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    if this.is_null() {
        return false;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER || (r.desc.flags & 0x7) != 0 {
        return false;
    }
    let ptr = r.data() as usize;
    if ptr == 0 {
        return false;
    }
    let span = footprint(&r.desc).map_or(0, |i| i.size) as usize;
    if span == 0 {
        return false;
    }
    let end = ptr.saturating_add(span);
    let mut cur = ptr;
    while cur < end {
        // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain Win32 C struct.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: plain VirtualQuery on a title address.
        let queried = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
        if queried == 0 || info.RegionSize == 0 {
            break;
        }
        let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        let chunk_end = region_end.min(end);
        let readable = info.State == MEM_COMMIT
            && (info.Protect & (PAGE_NOACCESS | PAGE_GUARD)) == 0
            && info.Protect != 0;
        if readable && chunk_end > cur {
            let len = chunk_end - cur;
            // SAFETY: VirtualQuery confirmed `[cur, chunk_end)` is committed
            // and readable (neither PAGE_NOACCESS nor PAGE_GUARD).
            let bytes = unsafe { std::slice::from_raw_parts(cur as *const u8, len) };
            // SAFETY: `align_to::<u64>` only reinterprets aligned interior bytes.
            let (prefix, words, suffix) = unsafe { bytes.align_to::<u64>() };
            if prefix.iter().any(|&b| b != 0)
                || words.iter().any(|&w| w != 0)
                || suffix.iter().any(|&b| b != 0)
            {
                return true;
            }
        }
        if region_end <= cur {
            break;
        }
        cur = region_end;
    }
    false
}

/// Runs `f` while no host GPU submission is executing (`Host::with_gpu_idle`),
/// or directly if no host GPU device has been created yet.
#[cfg(windows)]
fn with_host_gpu_idle<R>(f: impl FnOnce() -> R) -> R {
    match crate::host::existing_host() {
        Some(h) => h.with_gpu_idle(f),
        None => f(),
    }
}

/// The host GPU's copy of a title resource, materializing a deferred texture
/// on first GPU use.
#[cfg(windows)]
pub(crate) fn host_resource(
    this: This,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
        // Relieve commit pressure BEFORE acquiring `r.host` so eviction never
        // nests under a `Resource::host` lock (prevents AB-BA lock inversion).
        let est = r.deferred_bytes.load(std::sync::atomic::Ordering::Relaxed);
        if est > 0 && crate::memstat::under_pressure(est) {
            crate::shadow::relieve_pressure(est);
        }
    }
    let (res, newly_materialized) = {
        let mut slot = r.host.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(res) = &slot.resource {
            r.last_used.store(
                TEXTURE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::sync::atomic::Ordering::Relaxed,
            );
            return Some(res.clone());
        }
        if slot.failed {
            return None;
        }
        let Some((tex, host_bytes, initial_host_state)) =
            host_texture(&r.desc, r.initial_state, r.clear_value.as_ref())
        else {
            slot.failed = true;
            let prev = r
                .deferred_bytes
                .swap(0, std::sync::atomic::Ordering::Relaxed);
            if prev > 0 {
                crate::memstat::free(crate::memstat::Kind::DeferredTexture, prev);
            }
            return None;
        };
        let prev = r
            .deferred_bytes
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if prev > 0 {
            crate::memstat::free(crate::memstat::Kind::DeferredTexture, prev);
        }
        r.host_state
            .store(initial_host_state, std::sync::atomic::Ordering::Relaxed);
        r.last_used.store(
            TEXTURE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        slot.bytes = host_bytes;
        slot.twin = make_host_twin(&r.desc, &tex);
        slot.resource = Some(tex.clone());
        r.host_live
            .store(true, std::sync::atomic::Ordering::Release);
        crate::memstat::count(crate::memstat::Event::LazyMaterializations, 1);
        (tex, true)
    };
    if newly_materialized {
        // Backfill any shader-visible descriptor slots that deferred mirroring
        // while this texture was unmaterialized (lock already released).
        crate::descriptor::on_texture_materialized(this as usize);
    }
    Some(res)
}

/// The host texture unordered access views of a title texture name: the
/// twin of a depth target that has one, with the format such a view takes
/// when the title's names none or a depth format; else the host copy.
#[cfg(windows)]
pub(crate) fn host_unordered(
    this: This,
) -> Option<(
    windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    Option<u32>,
)> {
    let tex = host_resource(this)?;
    let r = resource(this);
    let slot = r.host.lock().unwrap_or_else(|e| e.into_inner());
    let Some(twin) = &slot.twin else {
        return Some((tex, None));
    };
    // SAFETY: a live resource.
    let typeless = unsafe { twin.texture.GetDesc() }.Format.0 as u32;
    let format = TWIN_FORMATS
        .iter()
        .find(|&&(f, _)| f == typeless)
        .map(|&(_, f)| f);
    Some((twin.texture.clone(), format))
}

/// Returns the already-materialized UAV host resource (the depth twin if one
/// exists, otherwise the host copy) without triggering lazy materialization.
#[cfg(windows)]
pub(crate) fn existing_host_unordered(
    this: This,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let slot = r.host.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(twin) = &slot.twin {
        return Some(twin.texture.clone());
    }
    slot.resource.clone()
}

/// Returns the unordered-access twin of a materialized depth target, if any.
#[cfg(windows)]
pub(crate) fn host_twin_of(this: This) -> Option<std::sync::Arc<HostTwin>> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let slot = r.host.lock().unwrap_or_else(|e| e.into_inner());
    slot.twin.clone()
}

/// Whether `format` is a 16-bit packed color format (`B5G6R5_UNORM`,
/// `B5G5R5A1_UNORM`, `B4G4R4A4_UNORM`) that console GPUs allow as render
/// targets / UAVs but many PC D3D12 devices reject for RT/UAV usage.
#[cfg(windows)]
pub(crate) fn is_packed_16bit_color(format: u32) -> bool {
    matches!(format, 85 | 86 | 115)
}

/// Shared-exponent RGB (`DXGI_FORMAT_R9G9B9E5_SHAREDEXP = 67`): consoles allow
/// RT/UAV usage, while PC D3D12 only allows SRV sampling.
#[cfg(windows)]
pub(crate) fn is_shared_exp_color(format: u32) -> bool {
    format == 67
}

/// Maps `format` to a host-supported DXGI format for `flags` using the host
/// device's `D3D12_FEATURE_FORMAT_SUPPORT`. Unsupported GPU-written (`RT`/
/// `UAV`) 16-bit packed color formats are promoted consistently to
/// `DXGI_FORMAT_B8G8R8A8_UNORM (87)`, and unsupported `R9G9B9E5_SHAREDEXP (67)`
/// RT/UAV textures are promoted to `DXGI_FORMAT_R16G16B16A16_FLOAT (10)`;
/// SRV-only textures stay native when supported.
#[cfg(windows)]
pub(crate) fn host_format_for(host: &crate::host::Host, format: u32, flags: u32) -> u32 {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
    let f = pc_format(format);
    let fallback = if is_packed_16bit_color(f) {
        87 // DXGI_FORMAT_B8G8R8A8_UNORM
    } else if is_shared_exp_color(f) {
        10 // DXGI_FORMAT_R16G16B16A16_FLOAT
    } else {
        return f;
    };
    const RENDER_TARGET: u32 = 1;
    const UNORDERED: u32 = 4;
    let (s1, _) = host.format_support(DXGI_FORMAT(f as i32));
    let required = if flags & (RENDER_TARGET | UNORDERED) != 0 {
        D3D12_FORMAT_SUPPORT1_TEXTURE2D.0
            | D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0
            | D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0
    } else {
        D3D12_FORMAT_SUPPORT1_TEXTURE2D.0 | D3D12_FORMAT_SUPPORT1_SHADER_SAMPLE.0
    };
    if s1.0 & required == required {
        f
    } else {
        fallback
    }
}

/// Render-target format matching [`host_format_for`] for graphics PSO
/// `RTVFormats`.
#[cfg(windows)]
pub(crate) fn host_rt_format(format: u32) -> u32 {
    match crate::host::host() {
        Some(host) => host_format_for(host, format, 1 | 4),
        None => pc_format(format),
    }
}

/// Host-compatible texture description derived from a console `ResourceDesc`.
#[cfg(windows)]
fn host_texture_desc(host: &crate::host::Host, desc: &ResourceDesc) -> ResourceDesc {
    const PC_FLAGS: u32 = 0xf; // render target, depth, UAV, no shader access
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    const DENY_SHADER_RESOURCE: u32 = 8;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags &= PC_FLAGS;
    // PC refuses depth targets that are also unordered-access (the console
    // allows both; Grand Theft Auto VI's shadow maps ask for it).
    // shortcut: the host copy loses UAV use; give it a second view-only
    // copy if draws write it as a UAV.
    if d.flags & DEPTH != 0 {
        d.flags &= !UNORDERED;
    }
    d.format = host_format_for(host, d.format, d.flags);
    if d.flags & DENY_SHADER_RESOURCE == 0
        && let Some(&(_, typeless)) = DEPTH_FORMATS.iter().find(|&&(f, _)| f == d.format)
    {
        d.format = typeless;
    }
    // The host has no sRGB unordered access: such textures are typeless,
    // their unordered access views plain UNORM.
    if d.flags & UNORDERED != 0 && SRGB.iter().any(|&(srgb, _)| srgb == d.format) {
        d.format = family(d.format);
    }
    d
}

/// A host texture like the title's: same description, in GPU memory,
/// without the console's layout and flags. Returns `(resource, bytes, initial_host_state)`.
#[cfg(windows)]
fn host_texture(
    desc: &ResourceDesc,
    state: u32,
    clear: Option<&[u32; 5]>,
) -> Option<(
    windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    u64,
    u32,
)> {
    use windows::Win32::Graphics::Direct3D12::*;
    if desc.dimension == BUFFER {
        return None;
    }
    let host = crate::host::host()?;
    let title_format = pc_format(desc.format);
    let d = host_texture_desc(host, desc);
    if d.format != title_format
        && (is_packed_16bit_color(title_format) || is_shared_exp_color(title_format))
    {
        tracing::debug!(
            from = title_format,
            to = d.format,
            flags = d.flags,
            width = d.width,
            height = d.height,
            "promoted unsupported texture format on host"
        );
    }
    // The clear value names the format too: format, then color or depth
    // and stencil (5 words).
    let clear_value = clear.map(|v| {
        let mut value = *v;
        value[0] = host_format_for(host, value[0], d.flags);
        value
    });
    let initial_host_state = crate::host::legal_for(d.flags, crate::host::texture_state(state));
    // SAFETY: a plain query with the description the texture will be made of.
    let est_size = unsafe {
        host.device
            .GetResourceAllocationInfo(0, &[*(&raw const d).cast::<D3D12_RESOURCE_DESC>()])
    }
    .SizeInBytes;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: the description has the host layout (checked size, same
    // fields); the out pointer is ours.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            (&raw const d).cast(),
            D3D12_RESOURCE_STATES(initial_host_state as i32),
            // The same layout: format, then color or depth and stencil.
            clear_value.as_ref().map(|v| std::ptr::from_ref(v).cast()),
            &mut texture,
        )
    };
    if let Err(e) = made {
        static FIRST_ERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !FIRST_ERR.swap(true, std::sync::atomic::Ordering::Relaxed) {
            // SAFETY: a plain query on a live device.
            let removed = unsafe { host.device.GetDeviceRemovedReason() }.err();
            let dbg = host.log_messages();
            tracing::error!(
                error = %e,
                removed = ?removed,
                debug_layer = ?dbg,
                thread = ?std::thread::current().id(),
                frame = crate::queue::presented(),
                pipelines_making = crate::pipeline::making(),
                desc = ?d,
                "first host texture creation failure"
            );
        } else {
            tracing::warn!(error = %e, desc = ?d, "no host texture");
        }
        crate::memstat::count(crate::memstat::Event::Failures, 1);
    }
    let t = texture?;
    crate::host::track(&t, crate::memstat::Kind::HostTexture, est_size);
    crate::host::name(&t, || {
        format!(
            "texture {}x{} f{} flags {:#x}",
            d.width, d.height, d.format, d.flags
        )
    });
    Some((t, est_size, initial_host_state))
}

/// Depth families that have unordered access formats (16-bit and 32-bit
/// depth; the stencil ones have none), and the format with the depth
/// format's bits: R16_UNORM for D16_UNORM, R32_FLOAT for D32_FLOAT.
#[cfg(windows)]
const TWIN_FORMATS: [(u32, u32); 2] = [(53, 56), (39, 41)];

/// Creates an unordered-access twin for `depth` when the title requested both
/// `ALLOW_DEPTH_STENCIL` and `ALLOW_UNORDERED_ACCESS`.
#[cfg(windows)]
fn make_host_twin(
    desc: &ResourceDesc,
    depth: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
) -> Option<std::sync::Arc<HostTwin>> {
    const BOTH: u32 = 2 | 4;
    if desc.flags & BOTH != BOTH {
        return None;
    }
    let texture = host_twin(depth)?;
    Some(std::sync::Arc::new(HostTwin {
        texture,
        state: 0.into(),
    }))
}

/// A twin for the unordered access of host depth target `depth`: its
/// description with unordered access instead of depth use, in the
/// typeless format of its family, so that views of the twin take any
/// format of the family (the title's own). Copies between the two keep
/// the bits (CopyResource takes depth resources and formats of one type
/// group). `None` without such a family (stencil), multisampled, or not
/// 2D.
#[cfg(windows)]
fn host_twin(
    depth: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    use windows::Win32::Graphics::Direct3D12::*;
    let host = crate::host::host()?;
    // SAFETY: a live resource.
    let mut d = unsafe { depth.GetDesc() };
    let typeless = family(d.Format.0 as u32);
    if d.SampleDesc.Count > 1
        || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || !TWIN_FORMATS.iter().any(|&(f, _)| f == typeless)
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                format = d.Format.0,
                samples = d.SampleDesc.Count,
                "no unordered access twin for this depth target: its UAV writes are dropped"
            )
        });
        return None;
    }
    d.Format.0 = typeless as i32;
    d.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // SAFETY: plain query with the twin's description.
    let est_size = unsafe { host.device.GetResourceAllocationInfo(0, &[d]) }.SizeInBytes;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut twin: Option<ID3D12Resource> = None;
    // SAFETY: the host's own description, changed to a legal one; the out
    // pointer is ours.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &d,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut twin,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, desc = ?d, "no unordered access twin");
    }
    let t = twin?;
    crate::host::track(&t, crate::memstat::Kind::HostTexture, est_size);
    crate::host::name(&t, || {
        format!("depth-uav-twin {}x{} f{}", d.Width, d.Height, d.Format.0)
    });
    Some(t)
}

/// A texture's console memory and description; `None` for buffers.
pub(crate) fn texture_memory(this: This) -> Option<(*const u8, ResourceDesc)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension != BUFFER).then(|| (r.data().cast_const(), r.desc))
}

/// A buffer's host memory and size; `None` for textures.
pub(crate) fn buffer_bytes(this: This) -> Option<(*mut u8, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension == BUFFER).then(|| (r.data(), r.desc.width))
}

impl Resource {
    fn data(&self) -> *mut u8 {
        match self.backing {
            Backing::Committed(ref memory) => memory.ptr.as_ptr(),
            // SAFETY: placement checked the range lies inside the heap.
            Backing::Placed { heap, offset } => unsafe {
                (*heap.cast::<Heap>())
                    .memory
                    .ptr
                    .as_ptr()
                    .add(offset as usize)
            },
            Backing::Address(at) => at,
        }
    }
}

fn resource<'a>(this: This) -> &'a Resource {
    // SAFETY: called from resource methods only.
    unsafe { &*this.cast::<Resource>() }
}

// The creation methods' own arguments, passed on.
#[allow(clippy::too_many_arguments)]
fn hand_out_resource(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    hand_out_resource_inner(
        desc, heap, heap_flags, backing, state, clear, iid, out, true,
    )
}

#[allow(clippy::too_many_arguments)]
fn hand_out_resource_inner(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
    track_evictable: bool,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = (state, clear, track_evictable);
    #[cfg(windows)]
    let clear_value = (!clear.is_null()).then(|| {
        // SAFETY: the title passes a whole 20-byte clear value.
        unsafe { clear.cast::<[u32; 5]>().read_unaligned() }
    });
    #[cfg(windows)]
    let (slot, live, initial_host_state, deferred_est) = if desc.dimension == BUFFER {
        (HostTexSlot::default(), false, 0, 0)
    } else if !lazy_textures_enabled() {
        let est = footprint(&desc).map_or(KIB_64, |i| i.size);
        if crate::memstat::under_pressure(est) {
            crate::shadow::relieve_pressure(est);
        }
        match host_texture(&desc, state, clear_value.as_ref()) {
            Some((tex, bytes, host_state)) => {
                let twin = make_host_twin(&desc, &tex);
                (
                    HostTexSlot {
                        resource: Some(tex),
                        twin,
                        failed: false,
                        bytes,
                    },
                    true,
                    host_state,
                    0,
                )
            }
            None => (
                HostTexSlot {
                    resource: None,
                    twin: None,
                    failed: true,
                    bytes: 0,
                },
                false,
                0,
                0,
            ),
        }
    } else {
        let est = footprint(&desc).map_or(KIB_64, |i| i.size);
        crate::memstat::alloc(crate::memstat::Kind::DeferredTexture, est);
        let initial_host_state =
            crate::host::legal_for(desc.flags & 0xf, crate::host::texture_state(state));
        (HostTexSlot::default(), false, initial_host_state, est)
    };
    let resource = Box::new(Resource {
        object: Object::new(&RESOURCE_FUNCTIONS),
        desc,
        heap,
        heap_flags,
        backing,
        #[cfg(windows)]
        host: std::sync::Mutex::new(slot),
        #[cfg(windows)]
        host_live: std::sync::atomic::AtomicBool::new(live),
        #[cfg(windows)]
        initial_state: state,
        #[cfg(windows)]
        clear_value,
        #[cfg(windows)]
        deferred_bytes: std::sync::atomic::AtomicU64::new(deferred_est),
        #[cfg(windows)]
        host_state: initial_host_state.into(),
        #[cfg(windows)]
        last_used: std::sync::atomic::AtomicU64::new(0),
        #[cfg(windows)]
        list_guard: std::sync::Arc::new(()),
    });
    #[cfg(windows)]
    {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        if desc.dimension == BUFFER {
            crate::shadow::register(at, resource.data() as u64, desc.width);
        } else if track_evictable && (desc.flags & 0x7) == 0 {
            TITLE_WRITTEN_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at);
        } else if (desc.flags & 0x7) != 0 {
            GPU_TARGET_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at);
        }
    }
    object::hand_out(RESOURCE, resource, iid, &RESOURCE_IIDS, out)
}

#[cfg(windows)]
impl Drop for Resource {
    fn drop(&mut self) {
        let at = std::ptr::from_ref::<Resource>(self) as usize;
        if self.desc.dimension != BUFFER {
            TITLE_WRITTEN_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&at);
            GPU_TARGET_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&at);
        }
        let deferred = self
            .deferred_bytes
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if deferred > 0 {
            crate::memstat::free(crate::memstat::Kind::DeferredTexture, deferred);
        }
        crate::texture::forget(at);
        crate::descriptor::forget(at);
        if self.desc.dimension == BUFFER {
            crate::shadow::unregister(at, self.data() as u64);
        }
    }
}

pub(crate) extern "system" fn create_committed(
    _: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes both descriptions (null is tolerated).
    let (Some(&heap), Some(&desc)) = (unsafe { (heap.as_ref(), desc.as_ref()) }) else {
        return E_INVALIDARG;
    };
    let Some(info) = footprint(&desc) else {
        tracing::warn!(?desc, "no size for this resource");
        return E_INVALIDARG;
    };
    tracing::debug!(
        dimension = desc.dimension,
        size = info.size,
        heap = heap.kind,
        "committed resource"
    );
    let memory = match Memory::for_resource(info, heap.kind) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    hand_out_resource(
        desc,
        heap,
        heap_flags,
        Backing::Committed(memory),
        state,
        clear,
        iid,
        out,
    )
}

/// Device method 0x3f8, inferred from Grand Theft Auto VI's calls
/// (title+0x193e1b4: its back buffers, heap flag "allow display"): the
/// committed resource creation of the enhanced-barrier interfaces,
/// `(heap, heap flags, desc1, initial layout, clear value, protected
/// session, castable format count, castable formats, iid, out)`. The host
/// copy starts in the common state, as layouts are not tracked.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn create_committed3(
    this: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    _layout: u32,
    clear: *const c_void,
    _protected: This,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    const COMMON: u32 = 0;
    create_committed(this, heap, heap_flags, desc, COMMON, clear, iid, out)
}

/// Registered page pools (64 KiB pages the title maps into tiled
/// resources by page index): first address and page count; a handle is an
/// index plus one, a removed pool leaves `None`.
static PAGE_POOLS: std::sync::Mutex<Vec<Option<(u64, u32)>>> = std::sync::Mutex::new(Vec::new());

fn page_pools() -> std::sync::MutexGuard<'static, Vec<Option<(u64, u32)>>> {
    PAGE_POOLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method 0x250: `(address, page count, handle out)`.
pub(crate) extern "system" fn register_page_pool(
    _: This,
    address: u64,
    pages: u32,
    out: *mut usize,
) -> HRESULT {
    if out.is_null() || pages == 0 || !address.is_multiple_of(KIB_64) {
        return E_INVALIDARG;
    }
    tracing::debug!(address = format_args!("{address:#x}"), pages, "page pool");
    let mut pools = page_pools();
    pools.push(Some((address, pages)));
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(pools.len()) };
    S_OK
}

/// Device method 0x258.
pub(crate) extern "system" fn unregister_page_pool(_: This, handle: usize) -> HRESULT {
    match page_pools().get_mut(handle.wrapping_sub(1)) {
        Some(pool @ Some(_)) => {
            *pool = None;
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

/// Device method 0x428, one of the `ID3D12Device9` to `ID3D12Device11`
/// additions. From Grand Theft Auto VI's call: a resource at a virtual
/// address the title mapped itself, `(address, desc1, initial barrier
/// layout, clear value, iid, out)`, as `CreatePlacedResourceX` with
/// enhanced barriers. GPU addresses are CPU addresses, so the resource is
/// a view of that memory. `D3D12_RESOURCE_DESC1` starts like the plain
/// description; its sampler feedback region is not needed.
pub(crate) extern "system" fn create_at_address(
    _: This,
    address: u64,
    desc: *const ResourceDesc,
    layout: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if address == 0 {
        return E_INVALIDARG;
    }
    tracing::debug!(
        address = format_args!("{address:#x}"),
        dimension = desc.dimension,
        width = desc.width,
        layout,
        "resource at an address"
    );
    const COMMON: u32 = 0;
    const DEFAULT: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 1,
        visible_nodes: 1,
    };
    hand_out_resource(
        desc,
        DEFAULT,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        clear,
        iid,
        out,
    )
}

/// Live title-written textures (no RT/DSV/UAV flags) eligible for safe idle
/// host-texture eviction under commit pressure because title memory still
/// holds their authoritative pixels.
#[cfg(windows)]
static TITLE_WRITTEN_TEXTURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::LazyLock::new(Default::default);

/// Live GPU-written textures (`flags & 0x7 != 0`: RT, DSV, UAV), tracked for
/// pressure diagnostics (aliasing overlap, staleness, top allocations).
#[cfg(windows)]
static GPU_TARGET_TEXTURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<usize>>,
> = std::sync::LazyLock::new(Default::default);

/// Cached entry for a texture placed in title memory without a resource object.
#[cfg(windows)]
struct PlacedEntry {
    resource: usize,
    title_span: u64,
    host_bytes: u64,
    last_used: u64,
}

/// Budget for cached `PLACED_TEXTURES` host textures before LRU idle eviction.
#[cfg(windows)]
const PLACED_TEXTURE_BUDGET: u64 = 128 << 20;

/// Textures the title placed in its own memory and views without a
/// resource object (device 0x230, Grand Theft Auto VI's streamed and
/// interface textures), by address and description: a resource at that
/// address, made on first use, invalidated on title memory remapping, and
/// bounded by [`PLACED_TEXTURE_BUDGET`] / commit pressure.
#[cfg(windows)]
static PLACED_TEXTURES: std::sync::Mutex<
    std::collections::BTreeMap<(u64, [u32; 14]), PlacedEntry>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Lock-free count of live `PLACED_TEXTURES` entries so frequent
/// `on_title_memory_remapped()` calls skip locking when the cache is empty.
#[cfg(windows)]
static PLACED_TEXTURE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Placed textures evicted from `PLACED_TEXTURES` while an open command list
/// still referenced them; released as soon as their list resets.
#[cfg(windows)]
static RETIRED_PLACED: std::sync::Mutex<Vec<(usize, u64)>> = std::sync::Mutex::new(Vec::new());

/// Releases a placed-texture `Resource` if no open command list or bindless
/// heap holds it (and while the host GPU is idle), else defers release into
/// `RETIRED_PLACED`. Returns host bytes freed now.
#[cfg(windows)]
fn release_placed_entry(res: usize, host_bytes: u64) -> u64 {
    with_host_gpu_idle(|| {
        let r = resource(res as This);
        if std::sync::Arc::strong_count(&r.list_guard) == 1
            && !crate::descriptor::is_texture_pinned_by_bindless(res)
        {
            crate::descriptor::evict_texture_slots(res, None, false);
            resource_release(res as This);
            if host_bytes > 0 {
                crate::memstat::count(crate::memstat::Event::TexturesEvicted, 1);
                crate::memstat::count(crate::memstat::Event::TexturesEvictedBytes, host_bytes);
            }
            host_bytes
        } else {
            RETIRED_PLACED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((res, host_bytes));
            0
        }
    })
}

/// Drains any previously retired placed textures whose command lists and
/// bindless heaps are now idle (while the host GPU is idle). Returns host
/// bytes freed.
#[cfg(windows)]
fn drain_retired_placed() -> u64 {
    if RETIRED_PLACED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
    {
        return 0;
    }
    with_host_gpu_idle(|| {
        let mut retired = RETIRED_PLACED.lock().unwrap_or_else(|e| e.into_inner());
        let mut ready = Vec::new();
        let mut i = 0;
        while i < retired.len() {
            let (res, _bytes) = retired[i];
            let r = resource(res as This);
            if std::sync::Arc::strong_count(&r.list_guard) == 1
                && !crate::descriptor::is_texture_pinned_by_bindless(res)
            {
                ready.push(retired.swap_remove(i));
            } else {
                i += 1;
            }
        }
        drop(retired);
        let mut freed = 0u64;
        for (res, bytes) in ready {
            crate::descriptor::evict_texture_slots(res, None, false);
            resource_release(res as This);
            if bytes > 0 {
                crate::memstat::count(crate::memstat::Event::TexturesEvicted, 1);
                crate::memstat::count(crate::memstat::Event::TexturesEvictedBytes, bytes);
                freed += bytes;
            }
        }
        freed
    })
}

/// Evicts LRU idle entries from `PLACED_TEXTURES` until live bytes fit within
/// `budget`. Returns host bytes freed.
#[cfg(windows)]
fn reclaim_placed_textures(budget: u64) -> u64 {
    let mut freed = drain_retired_placed();
    let evicted: Vec<(usize, u64)> = {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let total: u64 = map.values().map(|e| e.host_bytes).sum();
        if total <= budget {
            return freed;
        }
        let mut candidates: Vec<(u64, (u64, [u32; 14]), u64)> = map
            .iter()
            .filter(|(_, e)| {
                std::sync::Arc::strong_count(&resource(e.resource as This).list_guard) == 1
                    && !crate::descriptor::is_texture_pinned_by_bindless(e.resource)
            })
            .map(|(k, e)| (e.last_used, *k, e.host_bytes))
            .collect();
        candidates.sort_unstable_by_key(|c| c.0);
        let mut over = total - budget;
        let mut out = Vec::new();
        for (_, key, bytes) in candidates {
            if over == 0 {
                break;
            }
            if let Some(entry) = map.remove(&key) {
                PLACED_TEXTURE_COUNT.store(map.len(), std::sync::atomic::Ordering::Relaxed);
                out.push((entry.resource, entry.host_bytes));
                over = over.saturating_sub(bytes);
            }
        }
        out
    };
    for (res, bytes) in evicted {
        freed += release_placed_entry(res, bytes);
    }
    freed
}

/// Called when title memory in `[at, at + len)` is unmapped or remapped:
/// evicts any cached `PLACED_TEXTURES` overlapping that address range so
/// streamed/reused memory never leaks stale host textures.
#[cfg(windows)]
pub(crate) fn on_title_memory_remapped(at: u64, len: u64) {
    let Some(end) = at.checked_add(len).filter(|&e| e > at) else {
        return;
    };
    if PLACED_TEXTURE_COUNT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        drain_retired_placed();
        return;
    }
    let evicted: Vec<(usize, u64)> = {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(u64, [u32; 14])> = map
            .iter()
            .filter(|&(&(addr, _), entry)| {
                let tex_end = addr.saturating_add(entry.title_span.max(1));
                addr < end && tex_end > at
            })
            .map(|(k, _)| *k)
            .collect();
        let out: Vec<_> = keys
            .into_iter()
            .filter_map(|k| map.remove(&k).map(|e| (e.resource, e.host_bytes)))
            .collect();
        PLACED_TEXTURE_COUNT.store(map.len(), std::sync::atomic::Ordering::Relaxed);
        out
    };
    for (res, bytes) in evicted {
        release_placed_entry(res, bytes);
    }
    drain_retired_placed();
}

/// Evicts idle placed textures and idle materialized title-written host
/// textures (whose authoritative pixels still live in title memory) to
/// relieve system-commit pressure. Never evicts GPU-written textures (RT,
/// DSV, UAV, or copy/resolve targets), any texture referenced by an open or
/// executing command list, or any texture bound in a shader-visible heap
/// with an active bindless list. Returns host bytes freed.
#[cfg(windows)]
pub(crate) fn evict_idle_textures(incoming: u64) -> u64 {
    use windows::core::Interface;
    let (placed_live_bytes, placed_list_pinned_bytes, placed_bindless_pinned_bytes) = {
        let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let mut live = 0u64;
        let mut list_pin = 0u64;
        let mut bindless_pin = 0u64;
        for e in map.values() {
            live += e.host_bytes;
            if std::sync::Arc::strong_count(&resource(e.resource as This).list_guard) != 1 {
                list_pin += e.host_bytes;
            } else if crate::descriptor::is_texture_pinned_by_bindless(e.resource) {
                bindless_pin += e.host_bytes;
            }
        }
        (live, list_pin, bindless_pin)
    };
    let placed_freed = reclaim_placed_textures(0);
    let mut freed = placed_freed;
    let (
        mut candidates,
        title_live_bytes,
        title_list_pinned,
        title_host_written,
        title_bindless_pinned,
    ) = {
        let set = TITLE_WRITTEN_TEXTURES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        let (mut live, mut list_pin, mut host_wr, mut bindless_pin) = (0u64, 0u64, 0u64, 0u64);
        for &res in set.iter() {
            let r = resource(res as This);
            if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            let bytes = r
                .host
                .try_lock()
                .map(|s| s.bytes)
                .unwrap_or_else(|_| footprint(&r.desc).map_or(KIB_64, |i| i.size));
            live += bytes;
            if std::sync::Arc::strong_count(&r.list_guard) != 1 {
                list_pin += bytes;
                continue;
            }
            if crate::texture::is_host_written(res as This) {
                host_wr += bytes;
                continue;
            }
            if crate::descriptor::is_texture_pinned_by_bindless(res) {
                bindless_pin += bytes;
                continue;
            }
            out.push((r.last_used.load(std::sync::atomic::Ordering::Relaxed), res));
        }
        (out, live, list_pin, host_wr, bindless_pin)
    };
    let mut title_freed = 0u64;
    if incoming == 0 || freed < incoming {
        candidates.sort_unstable_by_key(|c| c.0);
        for (_, res) in candidates {
            if incoming > 0 && freed >= incoming {
                break;
            }
            // Hold GPU-idle + `TITLE_WRITTEN_TEXTURES` across this single
            // candidate's eviction so:
            // 1. No GPU submission executes while descriptors or textures change.
            // 2. `Resource::drop` (which removes from `TITLE_WRITTEN_TEXTURES`
            //    first) cannot free `Box<Resource>` concurrently.
            // 3. `r.host` is taken FIRST via `try_lock()` (never deadlocks) and
            //    dropped before `evict_texture_slots(res, Some(raw_ptr), true)`,
            //    so concurrent re-materialization of a new host texture is never
            //    clobbered.
            let dropped = with_host_gpu_idle(|| {
                let set = TITLE_WRITTEN_TEXTURES
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !set.contains(&res) {
                    return None;
                }
                let r = resource(res as This);
                if std::sync::Arc::strong_count(&r.list_guard) != 1
                    || crate::texture::is_host_written(res as This)
                    || crate::descriptor::is_texture_pinned_by_bindless(res)
                {
                    return None;
                }
                let taken = {
                    let Ok(mut slot) = r.host.try_lock() else {
                        return None;
                    };
                    if std::sync::Arc::strong_count(&r.list_guard) != 1 {
                        None
                    } else if let Some(tex) = slot.resource.take() {
                        let _twin = slot.twin.take();
                        let bytes = slot.bytes;
                        slot.bytes = 0;
                        r.host_live
                            .store(false, std::sync::atomic::Ordering::Release);
                        let est = footprint(&r.desc).map_or(KIB_64, |i| i.size);
                        let prev = r
                            .deferred_bytes
                            .swap(est, std::sync::atomic::Ordering::Relaxed);
                        if prev == 0 {
                            crate::memstat::alloc(crate::memstat::Kind::DeferredTexture, est);
                        }
                        crate::texture::reset_uploaded(res);
                        Some((tex, bytes))
                    } else {
                        None
                    }
                };
                if let Some((ref tex, _)) = taken {
                    crate::descriptor::evict_texture_slots(res, Some(tex.as_raw() as usize), true);
                }
                taken
            });
            if let Some((tex, bytes)) = dropped {
                drop(tex);
                freed += bytes;
                title_freed += bytes;
                crate::memstat::count(crate::memstat::Event::TexturesEvicted, 1);
                crate::memstat::count(crate::memstat::Event::TexturesEvictedBytes, bytes);
            }
        }
    }
    let total_host_tex = crate::memstat::live(crate::memstat::Kind::HostTexture).0;
    if total_host_tex > 0 {
        let mib = |b: u64| b / (1 << 20);
        let gpu_targets = total_host_tex.saturating_sub(placed_live_bytes + title_live_bytes);
        tracing::info!(
            total_mb = mib(total_host_tex),
            gpu_targets_mb = mib(gpu_targets),
            placed_mb = mib(placed_live_bytes),
            placed_list_pin_mb = mib(placed_list_pinned_bytes),
            placed_bindless_pin_mb = mib(placed_bindless_pinned_bytes),
            placed_freed_mb = mib(placed_freed),
            title_mb = mib(title_live_bytes),
            title_list_pin_mb = mib(title_list_pinned),
            title_bindless_pin_mb = mib(title_bindless_pinned),
            title_host_written_mb = mib(title_host_written),
            title_freed_mb = mib(title_freed),
            "texture eviction breakdown"
        );
        log_gpu_targets_breakdown();
    }
    freed
}

/// Logs a concise diagnostic breakdown of live GPU-target textures (RT/DSV/UAV):
/// total MB, aliased address-overlap MB, RT/DSV/UAV split, idle/stale MB, and
/// the 5 largest textures. Runs only during commit-pressure relief.
#[cfg(windows)]
fn log_gpu_targets_breakdown() {
    struct TargetInfo {
        start: u64,
        end: u64,
        bytes: u64,
        flags: u32,
        width: u64,
        height: u32,
        format: u32,
        last_used: u64,
        idle: bool,
    }
    let now_tick = TEXTURE_TICK.load(std::sync::atomic::Ordering::Relaxed);
    let mut entries: Vec<TargetInfo> = Vec::new();
    {
        let set = GPU_TARGET_TEXTURES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for &res in set.iter() {
            let r = resource(res as This);
            if !r.host_live.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            let bytes = r
                .host
                .try_lock()
                .map(|s| s.bytes)
                .unwrap_or_else(|_| footprint(&r.desc).map_or(KIB_64, |i| i.size));
            let addr = r.data() as u64;
            let span = footprint(&r.desc).map_or(bytes, |i| i.size).max(1);
            let last_used = r.last_used.load(std::sync::atomic::Ordering::Relaxed);
            let idle = std::sync::Arc::strong_count(&r.list_guard) == 1;
            entries.push(TargetInfo {
                start: addr,
                end: addr.saturating_add(span),
                bytes,
                flags: r.desc.flags,
                width: r.desc.width,
                height: r.desc.height,
                format: r.desc.format,
                last_used,
                idle,
            });
        }
    }
    if entries.is_empty() {
        return;
    }
    // Sort by title address to compute overlapping (aliased) footprint bytes.
    entries.sort_unstable_by_key(|e| (e.start, e.end));
    let mut max_end = 0u64;
    let mut aliased_bytes = 0u64;
    let (mut rt_b, mut dsv_b, mut uav_b, mut stale_b, mut live_b) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for e in &entries {
        live_b += e.bytes;
        if e.flags & 1 != 0 {
            rt_b += e.bytes;
        }
        if e.flags & 2 != 0 {
            dsv_b += e.bytes;
        }
        if e.flags & 4 != 0 {
            uav_b += e.bytes;
        }
        if e.idle && now_tick.saturating_sub(e.last_used) > 1000 {
            stale_b += e.bytes;
        }
        if e.start != 0 && e.start < max_end {
            aliased_bytes += e.bytes;
        }
        if e.start != 0 && e.end > max_end {
            max_end = e.end;
        }
    }
    entries.sort_unstable_by_key(|e| std::cmp::Reverse(e.bytes));
    let top5: Vec<String> = entries
        .iter()
        .take(5)
        .map(|e| {
            format!(
                "{}x{} f{} fl{:#x} @{:#x} {}MB",
                e.width,
                e.height,
                e.format,
                e.flags,
                e.start,
                e.bytes / (1 << 20)
            )
        })
        .collect();
    let mib = |b: u64| b / (1 << 20);
    tracing::info!(
        count = entries.len(),
        live_mb = mib(live_b),
        aliased_mb = mib(aliased_bytes),
        rt_mb = mib(rt_b),
        dsv_mb = mib(dsv_b),
        uav_mb = mib(uav_b),
        stale_mb = mib(stale_b),
        ?top5,
        "gpu targets breakdown"
    );
}

/// Live reference to a placed texture returned by [`texture_at`]. Holds a clone
/// of the texture's `list_guard` acquired under `PLACED_TEXTURES` so concurrent
/// `reclaim_placed_textures` / `on_title_memory_remapped` cannot free the
/// underlying `Resource` while the caller is creating views or pinning it onto
/// a command list.
#[cfg(windows)]
pub(crate) struct PlacedGuard {
    resource: This,
    guard: std::sync::Arc<()>,
}

#[cfg(windows)]
impl PlacedGuard {
    #[cfg(test)]
    pub(crate) fn resource(&self) -> This {
        self.resource
    }

    pub(crate) fn into_parts(self) -> (This, std::sync::Arc<()>) {
        (self.resource, self.guard)
    }
}

/// The resource for a texture placed at `address` with the resource
/// description `desc` (as words), made on first use, paired with an active-use
/// guard acquired under `PLACED_TEXTURES` to prevent concurrent eviction UAF.
#[cfg(windows)]
pub(crate) fn texture_at(address: u64, desc: &[u32; 14]) -> Option<PlacedGuard> {
    {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(&(address, *desc)) {
            entry.last_used = TEXTURE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let res = entry.resource as This;
            let guard = resource(res).list_guard.clone();
            return Some(PlacedGuard {
                resource: res,
                guard,
            });
        }
    }
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER || address == 0 {
        return None;
    }
    let title_span = footprint(&d).map_or(KIB_64, |i| i.size);
    const COMMON: u32 = 0;
    const DEFAULT: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 1,
        visible_nodes: 1,
    };
    let mut out = std::ptr::null();
    // Placed cache entries are owned by `PLACED_TEXTURES` rather than
    // `TITLE_WRITTEN_TEXTURES` (passing `track_evictable = false` avoids both
    // double-eviction and locking `TITLE_WRITTEN_TEXTURES` inside `texture_at`).
    let made = hand_out_resource_inner(
        d,
        DEFAULT,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        std::ptr::null(),
        &RESOURCE_IIDS[0],
        &mut out,
        false,
    );
    if made != S_OK {
        return None;
    }
    if host_resource(out).is_none() {
        resource_release(out);
        return None;
    }
    let host_bytes = resource(out)
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .bytes;
    let tick = TEXTURE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut duplicate = None;
    let guard = {
        let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get_mut(&(address, *desc)) {
            existing.last_used = tick;
            duplicate = Some(out);
            out = existing.resource as This;
        } else {
            map.insert(
                (address, *desc),
                PlacedEntry {
                    resource: out as usize,
                    title_span,
                    host_bytes,
                    last_used: tick,
                },
            );
            PLACED_TEXTURE_COUNT.store(map.len(), std::sync::atomic::Ordering::Relaxed);
        }
        // Pin BEFORE dropping `PLACED_TEXTURES` so concurrent eviction cannot
        // release `out` before the caller finishes using it.
        resource(out).list_guard.clone()
    };
    if let Some(dup) = duplicate {
        resource_release(dup);
    } else {
        reclaim_placed_textures(PLACED_TEXTURE_BUDGET);
    }
    Some(PlacedGuard {
        resource: out,
        guard,
    })
}

pub(crate) extern "system" fn create_placed(
    _: This,
    heap: This,
    offset: u64,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if heap.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our heaps.
    let h = unsafe { &*heap.cast::<Heap>() };
    let fits = footprint(&desc).is_some_and(|info| {
        offset.is_multiple_of(info.alignment)
            && offset
                .checked_add(info.size)
                .is_some_and(|end| end <= h.desc.size)
    });
    if !fits {
        tracing::warn!(
            ?desc,
            offset,
            heap = h.desc.size,
            "placed resource does not fit"
        );
        return E_INVALIDARG;
    }
    object::add_ref(heap);
    let backing = Backing::Placed { heap, offset };
    hand_out_resource(
        desc,
        h.desc.properties,
        h.desc.flags,
        backing,
        state,
        clear,
        iid,
        out,
    )
}

extern "system" fn resource_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        RESOURCE,
        iid,
        object::is(iid, &RESOURCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn resource_release(this: This) -> u32 {
    object::release_boxed::<Resource>(this)
}
/// Memory is always mapped (subresources are not told apart yet); a null
/// `out` only asks for residency.
extern "system" fn map(this: This, _sub: u32, _read: *const c_void, out: *mut *mut u8) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(resource(this).data()) };
    }
    S_OK
}
extern "system" fn unmap(_: This, _sub: u32, _written: *const c_void) {}
extern "system" fn resource_desc(this: This, out: *mut ResourceDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(resource(this).desc) };
    }
}
/// Device method `GetCustomHeapProperties`, returning its struct through a
/// hidden first argument: what a standard heap type is as a custom heap.
/// Memory is unified and cache-coherent (as the architecture query says),
/// so every type is in pool L0, default heaps are not CPU-visible and the
/// others are write-back.
pub(crate) extern "system" fn custom_heap_properties(
    out: *mut HeapProperties,
    _: This,
    nodes: u32,
    kind: u32,
) -> *mut HeapProperties {
    const CUSTOM: u32 = 4;
    const NOT_AVAILABLE: u32 = 1;
    const WRITE_BACK: u32 = 3;
    const L0: u32 = 1;
    let nodes = nodes.max(1);
    let cpu_pages = match kind {
        1 => NOT_AVAILABLE,
        2 | 3 => WRITE_BACK,
        _ => 0,
    };
    let properties = HeapProperties {
        kind: CUSTOM,
        cpu_pages,
        pool: if cpu_pages == 0 { 0 } else { L0 },
        creation_nodes: nodes,
        visible_nodes: nodes,
    };
    // SAFETY: the title passes room for the result (null is tolerated).
    if let Some(out) = unsafe { out.as_mut() } {
        *out = properties;
    }
    out
}

extern "system" fn gpu_address(this: This) -> u64 {
    let r = resource(this);
    let at = r.data() as u64;
    // The console's GPU addresses fit in 40 bits; its ray-tracing builder
    // refuses others (diagnostics: which memory breaks that).
    #[cfg(feature = "diagnostics")]
    if at >> 40 != 0 {
        static ONCE: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];
        let backing = match r.backing {
            Backing::Committed(ref m) => format!("committed, attributes {:?}", m.attributes),
            Backing::Placed { offset, .. } => format!("placed in a heap at {offset:#x}"),
            Backing::Address(_) => "at a title address".into(),
        };
        ONCE[usize::from(r.desc.dimension == BUFFER)].call_once(|| {
            tracing::warn!(address = format_args!("{at:#x}"), backing, desc = ?r.desc, "a GPU address above 40 bits")
        });
    }
    at
}
extern "system" fn heap_properties(
    this: This,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> HRESULT {
    let r = resource(this);
    // SAFETY: either pointer may be null; otherwise the title passes room.
    unsafe {
        if let Some(p) = properties.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

/// IUnknown 3, object 4, device child 1, heap 1.
static HEAP_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<HEAP, 24>());
    f[0] = heap_query as *const ();
    f[2] = heap_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = heap_desc as *const (); // 0x40
    Table(f)
};

/// IUnknown 3, object 4, device child 1, resource 10 (7 as on PC, then 3
/// console ones).
static RESOURCE_FUNCTIONS: Table<32> = {
    let mut f = object::d3d12_object(object::missing_table::<RESOURCE, 32>());
    f[0] = resource_query as *const ();
    f[2] = resource_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = map as *const (); // 0x40
    f[9] = unmap as *const (); // 0x48
    f[10] = resource_desc as *const (); // 0x50
    f[11] = gpu_address as *const (); // 0x58
    f[14] = heap_properties as *const (); // 0x70
    Table(f)
};

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::object::slot;

    type Release = extern "system" fn(This) -> u32;
    type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
    type Address = extern "system" fn(This) -> u64;

    const PROPERTIES: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };

    fn buffer(width: u64) -> ResourceDesc {
        ResourceDesc {
            dimension: BUFFER,
            alignment: 0,
            width,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags: 0,
        }
    }

    fn texture(width: u64, height: u32, format: u32, mips: u16) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            width,
            height,
            format,
            mip_levels: mips,
            layout: 0,
            ..buffer(0)
        }
    }

    fn heap(size: u64) -> This {
        let desc = HeapDesc {
            size,
            properties: PROPERTIES,
            alignment: 0,
            flags: 0,
        };
        let mut heap = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
            S_OK
        );
        heap
    }

    #[test]
    fn heaps_are_zeroed_aligned_memory() {
        let heap = heap(1 << 20);
        // SAFETY: a heap we just made.
        let memory = unsafe { &(*heap.cast::<Heap>()).memory };
        assert_eq!(memory.ptr.as_ptr() as u64 % KIB_64, 0);
        let mut back = std::mem::MaybeUninit::<HeapDesc>::uninit();
        slot::<extern "system" fn(This, *mut HeapDesc)>(heap, 0x40)(heap, back.as_mut_ptr());
        // SAFETY: written above.
        assert_eq!(unsafe { back.assume_init() }.size, 1 << 20);
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 0);

        let bad = HeapDesc {
            size: 1 << 20,
            properties: PROPERTIES,
            alignment: 3,
            flags: 0,
        };
        let mut out = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &bad, &HEAP_IIDS[0], &mut out),
            E_INVALIDARG
        );
    }

    #[test]
    fn device_setup_heap_properties_and_resources_at_addresses() {
        let device = crate::device::device();
        // GetCustomHeapProperties (0xd0): the struct comes back through the
        // hidden first argument, which is also returned.
        type Custom =
            extern "system" fn(*mut HeapProperties, This, u32, u32) -> *mut HeapProperties;
        let mut p = PROPERTIES;
        let back = slot::<Custom>(device, 0xd0)(&mut p, device, 1, 2);
        assert_eq!(back, &raw mut p);
        assert_eq!(
            (
                p.kind,
                p.cpu_pages,
                p.pool,
                p.creation_nodes,
                p.visible_nodes
            ),
            (4, 3, 1, 1, 1),
            "upload: custom, write-back, L0"
        );
        slot::<Custom>(device, 0xd0)(&mut p, device, 1, 1);
        assert_eq!(p.cpu_pages, 1, "default: not CPU-visible");

        // 0x428: a buffer over memory the title owns, as GTA VI asks.
        type At = extern "system" fn(
            This,
            u64,
            *const ResourceDesc,
            u32,
            *const c_void,
            *const Iid,
            *mut This,
        ) -> HRESULT;
        let memory = vec![0u8; 0x1_0000];
        let address = memory.as_ptr() as u64;
        let mut r = std::ptr::null();
        let create = slot::<At>(device, 0x428);
        let desc = buffer(0x1_0000);
        let null = std::ptr::null();
        assert_eq!(
            create(
                device,
                address,
                &desc,
                u32::MAX,
                null,
                &RESOURCE_IIDS[0],
                &mut r
            ),
            0
        );
        assert_eq!(slot::<Address>(r, 0x58)(r), address);
        assert_eq!(slot::<Release>(r, 0x10)(r), 0);
        // The title's memory is still there after the release.
        assert_eq!(memory[0xffff], 0);
        assert_ne!(
            create(device, 0, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r),
            0
        );
    }

    #[test]
    fn sizes_cover_every_mip() {
        let size = |d: &ResourceDesc| footprint(d).map(|i| i.size);
        assert_eq!(size(&buffer(256)), Some(KIB_64));
        // Textures take their console layout and its alignment (4 KiB
        // standard swizzle when the description leaves it to the library):
        // 512x512 BC7 with 10 mips is 0x56000 bytes, 4 KiB aligned, as on
        // the console.
        let layout = |d: &ResourceDesc| texture_layout(d).unwrap().size;
        let rgba = texture(256, 256, 28, 0);
        assert_eq!(size(&rgba), Some(layout(&rgba).next_multiple_of(4096)));
        let bc7 = footprint(&texture(512, 512, 98, 10));
        let console = AllocationInfo {
            size: 0x56000,
            alignment: 0x1000,
        };
        assert_eq!(bc7, Some(console));
        assert!(layout(&rgba) >= 4 * (256 * 256 * 4) / 3, "every mip");
        // BC1 is 8 bytes per 4x4 block: 1024x1024, one mip = 512 KiB.
        assert_eq!(size(&texture(1024, 1024, 71, 1)), Some(512 << 10));
        assert_eq!(size(&texture(16, 16, 66, 1)), None, "1-bit format");
        // Linear layout (1) and an explicit mode (64 KiB display, 0x10a).
        let linear = ResourceDesc {
            layout: 1,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&linear).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            0
        );
        let display = ResourceDesc {
            layout: 0x10a,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&display).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            10
        );

        let mut info = AllocationInfo {
            size: 0,
            alignment: 0,
        };
        let descs = [buffer(100), texture(256, 256, 28, 0)];
        allocation_info(&mut info, std::ptr::null(), 0, 2, descs.as_ptr());
        assert_eq!(
            info,
            AllocationInfo {
                size: KIB_64 + size(&descs[1]).unwrap(),
                alignment: KIB_64
            }
        );
        allocation_info(&mut info, std::ptr::null(), 0, 1, &texture(1, 1, 999, 1));
        assert_eq!(info.size, u64::MAX);
    }

    #[test]
    fn upload_footprints_follow_the_pc_rules() {
        let null = std::ptr::null();
        let mut layouts = [NO_FOOTPRINT; 2];
        let (mut rows, mut sizes, mut total) = ([0; 2], [0; 2], 0);
        let mut footprints = |desc: &ResourceDesc, first, count| {
            let (l, r, s) = (layouts.as_mut_ptr(), rows.as_mut_ptr(), sizes.as_mut_ptr());
            copyable_footprints(null, desc, first, count, 0, l, r, s, &mut total);
            (layouts, rows, sizes, total)
        };
        // 100x60 RGBA8, 2 mips: 400-byte rows pitched to 512.
        let (l, r, s, t) = footprints(&texture(100, 60, 28, 2), 0, 2);
        assert_eq!((l[0].row_pitch, r[0], s[0]), (512, 60, 400));
        // Mip 1 (50x30, 200-byte rows) starts after 59 pitched rows and one
        // packed row, rounded up to 512.
        assert_eq!(l[1].offset, (512 * 59 + 400u64).next_multiple_of(512));
        assert_eq!((l[1].width, l[1].height, r[1]), (50, 30, 30));
        assert_eq!(t, l[1].offset + 256 * 29 + 200);
        // BC1: rows of 4x4 blocks, 8 bytes each.
        let (_, r, s, _) = footprints(&texture(64, 64, 71, 1), 0, 1);
        assert_eq!((r[0], s[0]), (16, 128));
        let (_, _, _, t) = footprints(&texture(64, 64, 71, 1), 1, 1);
        assert_eq!(t, u64::MAX, "no second subresource");
    }

    /// A 4x4 texture filled from a title buffer through our list and queue,
    /// then read back from the host GPU.
    #[cfg(windows)]
    /// Reads a 2D `width` x `height` RGBA8 host texture's mip 0 back.
    fn read_rgba8(image: This, width: u32, height: u32) -> Vec<u8> {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::Interface;
        let host = crate::host::host().expect("host");
        let pitch = (width * 4).next_multiple_of(256);
        let len = u64::from(pitch * height);
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let back = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        #[repr(C)]
        struct Location {
            resource: *const c_void,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width,
            height,
            depth: 1,
            row_pitch: pitch,
        };
        let out_to = Location {
            resource: readback.as_raw().cast_const(),
            kind: 1,
            footprint: rows,
        };
        let out_from = Location {
            resource: host_image.as_raw().cast_const(),
            kind: 0,
            footprint: rows,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                &host_image,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            back.list.CopyTextureRegion(
                (&raw const out_to).cast(),
                0,
                0,
                0,
                (&raw const out_from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is `len` bytes.
        let read = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), len as usize)
        };
        (0..height as usize)
            .flat_map(|y| read[y * pitch as usize..][..width as usize * 4].to_vec())
            .collect()
    }

    /// Serializes tests that check process-wide `texture::RECORDED` or invoke
    /// global `evict_idle_textures(0)` / `relieve_pressure(0)`.
    #[cfg(windows)]
    pub(crate) static TEXTURE_GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A title texture's memory reaches its host copy at the submission that
    /// samples it, again only when the memory changed, through an upload
    /// buffer lent for that submission (none is kept per texture).
    #[test]
    fn title_texture_contents_are_uploaded_when_they_change() {
        use std::sync::atomic::Ordering::Relaxed;
        let _serial = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let (width, height) = (64u32, 32u32);
        let mut image = null;
        let desc = texture(u64::from(width), height, 28, 1);
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &desc, 0, null.cast(), iid, &mut image),
            S_OK
        );
        let (memory, d) = texture_memory(image).unwrap();
        let layout = texture_layout(&d).unwrap().planes[0].layout.clone();
        let linear = |seed: u8| -> Vec<u8> {
            (0..width * height * 4)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                .collect()
        };
        let write = |seed: u8| {
            // SAFETY: the texture's memory, its layout's size.
            let tiled =
                unsafe { std::slice::from_raw_parts_mut(memory.cast_mut(), layout.size as usize) };
            let pitch = width as usize * 4;
            layout
                .tile(tiled, 0, 0, &linear(seed), pitch, pitch * height as usize)
                .unwrap();
        };
        let submit = || {
            let list = host.new_list().unwrap();
            crate::texture::upload(&list, image, &host_resource(image).unwrap());
            crate::list::ensure_state(&list, image, 0x40 | 0x80);
            list.close();
            let before = crate::texture::RECORDED.load(Relaxed);
            host.execute(&[&list]);
            crate::texture::RECORDED.load(Relaxed) - before
        };
        write(1);
        assert_eq!(submit(), 1, "first use uploads");
        assert_eq!(read_rgba8(image, width, height), linear(1));
        assert_eq!(submit(), 0, "unchanged memory uploads nothing");
        assert_eq!(
            read_rgba8(image, width, height),
            linear(1),
            "and keeps its contents"
        );
        write(2);
        assert_eq!(submit(), 1, "changed memory uploads again");
        assert_eq!(read_rgba8(image, width, height), linear(2));
        // Many changing submissions reuse lent buffers instead of growing.
        let made = crate::memstat::made(crate::memstat::Kind::TextureStaging);
        for seed in 3..43 {
            write(seed);
            assert_eq!(submit(), 1);
        }
        assert_eq!(read_rgba8(image, width, height), linear(42));
        let new = crate::memstat::made(crate::memstat::Kind::TextureStaging) - made;
        assert!(new <= 2, "{new} upload buffers made for 40 uploads");
        crate::host::assert_no_debug_errors(&host.device);
    }

    /// Synthetic benchmark (`--ignored --test-threads=1 --nocapture`): 48
    /// title textures of 1 MiB each uploaded once. Before Phase 11 each kept
    /// an upload buffer of its full footprint for good; now upload memory is
    /// lent per submission from a bounded pool.
    #[test]
    #[ignore]
    fn bench_texture_staging() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let mib = |b: i128| b as f64 / (1 << 20) as f64;
        let commit = || crate::memstat::system_commit().expect("commit").0 as i128;
        let private = || crate::memstat::process().expect("process").0 as i128;
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let (c0, p0) = (commit(), private());
        let mut old_retained = 0u64;
        let mut images = Vec::new();
        for n in 0..48u8 {
            let mut image = null;
            let desc = texture(512, 512, 28, 1);
            assert_eq!(
                create_committed(null, &PROPERTIES, 0, &desc, 0, null.cast(), iid, &mut image),
                S_OK
            );
            let (memory, d) = texture_memory(image).unwrap();
            let layout = texture_layout(&d).unwrap().planes[0].layout.clone();
            // SAFETY: the texture's memory, its layout's size.
            unsafe { memory.cast_mut().write_bytes(n + 1, layout.size as usize) };
            let host_texture = host_resource(image).unwrap();
            // SAFETY: a live resource; outputs for one subresource.
            let total = unsafe {
                let d = host_texture.GetDesc();
                let mut total = 0;
                host.device
                    .GetCopyableFootprints(&d, 0, 1, 0, None, None, None, Some(&mut total));
                total
            };
            old_retained += total;
            let list = host.new_list().unwrap();
            crate::texture::upload(&list, image, &host_texture);
            crate::list::ensure_state(&list, image, 0x40 | 0x80);
            list.close();
            host.execute(&[&list]);
            images.push(image);
        }
        let staging = crate::memstat::live(crate::memstat::Kind::TextureStaging);
        let line = format!(
            "texture-staging textures=48x1MiB old-permanent-staging={:.0}MiB now-staging-live={:.0}MiB in {} buffers (pool bound 256MiB); host-textures={:.0}MiB; commit-delta={:+.0}MiB private-delta={:+.0}MiB",
            mib(old_retained as i128),
            mib(staging.0 as i128),
            staging.1,
            mib(crate::memstat::live(crate::memstat::Kind::HostTexture).0 as i128),
            mib(commit() - c0),
            mib(private() - p0)
        );
        println!("{line}");
        crate::memstat::report(&format!("synthetic {line}"));
        assert!(staging.0 < old_retained);
    }

    /// Synthetic measurement (`--ignored --test-threads=1 --nocapture`):
    /// what 256 MiB of default-heap (video memory) textures cost in process
    /// private bytes, system commit and the GPU's local budget.
    #[test]
    #[ignore]
    fn bench_default_heap_commit() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host) = crate::host::host() else {
            return;
        };
        let mib = |b: i128| b as f64 / (1 << 20) as f64;
        let commit = || crate::memstat::system_commit().expect("commit").0 as i128;
        let private = || crate::memstat::process().expect("process").0 as i128;
        let local = || host.video_memory().map_or(0, |v| v[0].0 as i128);
        let (c0, p0, l0) = (commit(), private(), local());
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4096,
            Height: 4096,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let textures: Vec<ID3D12Resource> = (0..4)
            .map(|_| {
                let mut t: Option<ID3D12Resource> = None;
                // SAFETY: plain creation of a texture.
                unsafe {
                    host.device
                        .CreateCommittedResource(
                            &heap,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut t,
                        )
                        .unwrap();
                }
                t.unwrap()
            })
            .collect();
        let (c1, p1, l1) = (commit(), private(), local());
        drop(textures);
        let (c2, p2) = (commit(), private());
        // Repeated create/free cycles: bounded reuse or growth?
        let mut cycles = Vec::new();
        for _ in 0..10 {
            let again: Vec<ID3D12Resource> = (0..4)
                .map(|_| {
                    let mut t: Option<ID3D12Resource> = None;
                    // SAFETY: plain creation of a texture.
                    unsafe {
                        host.device
                            .CreateCommittedResource(
                                &heap,
                                D3D12_HEAP_FLAG_NONE,
                                &desc,
                                D3D12_RESOURCE_STATE_COMMON,
                                None,
                                &mut t,
                            )
                            .unwrap();
                    }
                    t.unwrap()
                })
                .collect();
            drop(again);
            cycles.push(format!("{:+.0}", mib(private() - p0)));
        }
        // An idle submission and a pause: deferred destruction completes.
        let list = host.new_list().unwrap();
        list.close();
        host.execute(&[&list]);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let p3 = private();
        let line = format!(
            "default-heap textures=4x64MiB: commit-delta={:+.0}MiB private-delta={:+.0}MiB gpu-local-delta={:+.0}MiB; released: commit-delta={:+.0}MiB private-delta={:+.0}MiB; private after each of 10 create/free cycles: {}; after an idle submission and 500 ms: {:+.0}MiB",
            mib(c1 - c0),
            mib(p1 - p0),
            mib(l1 - l0),
            mib(c2 - c0),
            mib(p2 - p0),
            cycles.join(" "),
            mib(p3 - p0)
        );
        println!("{line}");
        crate::memstat::report(&format!("synthetic {line}"));
    }

    #[test]
    fn buffer_data_reaches_the_host_texture() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        const COPY_DEST: u32 = 0x400;
        let (mut image, mut source) = (null, null);
        let rgba = texture(4, 4, 28, 1);
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &rgba,
                COPY_DEST,
                null.cast(),
                iid,
                &mut image
            ),
            S_OK
        );
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &buffer(1024),
                0,
                null.cast(),
                iid,
                &mut source
            ),
            S_OK
        );
        let (data, _) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer holds 1024 bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, 1024) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );

        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 4,
            height: 4,
            depth: 1,
            row_pitch: 256,
        };
        let to = Location {
            resource: image,
            kind: 0,
            footprint: PlacedFootprint { offset: 0, ..rows },
        };
        let from = Location {
            resource: source,
            kind: 1,
            footprint: rows,
        };
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
        slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
        slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

        // Read the host texture back into a readback buffer.
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let back = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        use windows::core::Interface;
        let out_to = Location {
            resource: readback.as_raw().cast_const().cast(),
            kind: 1,
            footprint: rows,
        };
        let out_from = Location {
            resource: host_image.as_raw().cast_const().cast(),
            kind: 0,
            footprint: to.footprint,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                &host_image,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            back.list.CopyTextureRegion(
                (&raw const out_to).cast(),
                0,
                0,
                0,
                (&raw const out_from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 16], bytes[at..at + 16], "row {row}");
        }
    }

    #[test]
    fn committed_and_placed_resources_map_at_their_gpu_address() {
        let null = std::ptr::null();
        let mut committed = null;
        let hr = create_committed(
            null,
            &PROPERTIES,
            0,
            &buffer(256),
            0,
            null.cast(),
            &RESOURCE_IIDS[0],
            &mut committed,
        );
        assert_eq!(hr, S_OK);
        let mut data = std::ptr::null_mut();
        assert_eq!(
            slot::<Map>(committed, 0x40)(committed, 0, null.cast(), &mut data),
            S_OK
        );
        assert_eq!(slot::<Address>(committed, 0x58)(committed), data as u64);
        // SAFETY: the buffer is at least 256 zeroed bytes.
        assert_eq!(unsafe { *data.add(255) }, 0);
        assert_eq!(slot::<Release>(committed, 0x10)(committed), 0);

        let heap = heap(4 * KIB_64);
        let place = |offset, desc: &ResourceDesc, out: &mut This| {
            create_placed(
                null,
                heap,
                offset,
                desc,
                0,
                null.cast(),
                &RESOURCE_IIDS[0],
                out,
            )
        };
        let mut placed = null;
        assert_eq!(place(KIB_64, &buffer(KIB_64), &mut placed), S_OK);
        // SAFETY: a heap we made.
        let base = unsafe { (*heap.cast::<Heap>()).memory.ptr.as_ptr() } as u64;
        assert_eq!(slot::<Address>(placed, 0x58)(placed), base + KIB_64);
        let mut other = null;
        assert_eq!(
            place(4 * KIB_64, &buffer(1), &mut other),
            E_INVALIDARG,
            "past the end"
        );
        assert_eq!(
            place(100, &buffer(1), &mut other),
            E_INVALIDARG,
            "misaligned"
        );
        // The resource keeps the heap alive.
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 1);
        assert_eq!(slot::<Release>(placed, 0x10)(placed), 0);
    }

    #[test]
    #[cfg(windows)]
    fn unsupported_16bit_rt_uav_textures_promote_consistently() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let heap = crate::descriptor::host::HostHeap::new_capped_for_test(
            host_gpu.device.clone(),
            16,
            16,
            4,
        )
        .expect("host heap");

        // Formats & flags observed failing CreateCommittedResource with 0x80070057:
        // 86 (B5G5R5A1_UNORM, RT|UAV), 115 (B4G4R4A4_UNORM, RT), and
        // 67 (R9G9B9E5_SHAREDEXP, RT|UAV).
        for (format, flags) in [(86u32, 5u32), (115u32, 1u32), (67u32, 5u32)] {
            let desc = ResourceDesc {
                dimension: 3, // TEXTURE2D
                alignment: 0,
                width: 64,
                height: 64,
                depth_or_array_size: 1,
                mip_levels: 1,
                format,
                sample_count: 1,
                sample_quality: 0,
                layout: 0,
                flags,
            };
            let clear = [format, 0, 0, 0, 0x3f80_0000];
            let mut tex: This = std::ptr::null();
            let hr = create_committed(
                std::ptr::null(),
                &PROPERTIES,
                0,
                &desc,
                0,
                clear.as_ptr().cast(),
                &RESOURCE_IIDS[0],
                &mut tex,
            );
            assert_eq!(hr, S_OK);
            let host_tex = host_resource(tex).expect("16-bit RT/UAV texture must get a host copy");
            // SAFETY: live texture.
            let actual_fmt = unsafe { host_tex.GetDesc() }.Format.0 as u32;
            assert_eq!(
                actual_fmt,
                host_rt_format(format),
                "PSO RTFormat must match promoted host texture format"
            );

            // Explicit RTV with title's original 16-bit format must succeed cleanly.
            let mut rtv_slot = [0u8; 128];
            let rtv_desc = [format, 4, 0, 0, 0, 0]; // TEXTURE2D
            crate::descriptor::create_rtv(
                std::ptr::null(),
                tex,
                &rtv_desc,
                rtv_slot.as_mut_ptr() as u64,
            );
            let record = crate::descriptor::rtv_record(rtv_slot.as_ptr() as u64).unwrap();
            let _ = host_gpu.rtv(&host_tex, record.desc().as_ref());

            // Explicit SRV with title's original 16-bit format must succeed without null fallback.
            // Layout: [0]=Format, [1]=ViewDimension, [2]=Mapping, [3]=pad,
            // [4]=MostDetailedMip, [5]=MipLevels.
            let srv_words = [
                format,
                4, // TEXTURE2D
                windows::Win32::Graphics::Direct3D12::D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                0,
                0, // MostDetailedMip
                1, // MipLevels
                0,
                0,
                0,
                0,
            ];
            heap.make_at(
                0,
                &crate::descriptor::View::Shader(tex as usize, Some(srv_words)),
            );
            if flags & 4 != 0 {
                let uav_words = [format, 4, 0, 0, 0, 0, 0, 0, 0, 0];
                heap.make_at(
                    1,
                    &crate::descriptor::View::Unordered(tex as usize, 0, Some(uav_words)),
                );
            }
            assert!(
                heap.stats().contains("null_fallbacks=0"),
                "promoted 16-bit views must not fall back to null: {}",
                heap.stats()
            );
            crate::host::assert_no_debug_errors(&host_gpu.device);
            assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        }
    }

    /// Creating textures and writing their SRVs into a shader-visible heap
    /// does not allocate D3D12 host textures until actual GPU use; first GPU
    /// use materializes only that texture and backfills its deferred slots.
    #[cfg(windows)]
    #[test]
    fn lazy_textures_defer_allocation_until_first_gpu_use() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let heap_iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        #[repr(C)]
        struct ExtendedHeap([u32; 4], [u64; 2]);
        let hdesc = ExtendedHeap([0, 64, 1, 0], [0, 0]);
        let mut sheap = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const hdesc.0).cast(),
                &heap_iid,
                &mut sheap
            ),
            S_OK
        );
        let title_heap = crate::descriptor::heap(sheap);
        let _host_heap = title_heap.host().expect("host view heap");
        let start = title_heap.start;

        let lazy_before = crate::memstat::events(crate::memstat::Event::LazyMaterializations);
        let backing = vec![0u8; 64 << 10];
        let mut textures = Vec::new();
        for i in 0..16u64 {
            let mut tex = null;
            let desc = ResourceDesc {
                flags: 1, // ALLOW_RENDER_TARGET
                ..texture(256, 256, 28, 1)
            };
            assert_eq!(
                create_at_address(
                    null,
                    backing.as_ptr() as u64,
                    &desc,
                    0,
                    null.cast(),
                    iid,
                    &mut tex
                ),
                S_OK
            );
            assert!(
                !has_host_resource(tex),
                "texture {i} must stay deferred at creation"
            );
            assert!(existing_host_resource(tex).is_none());
            crate::descriptor::create_srv(null, tex, std::ptr::null(), start + i * 32);
            assert!(
                !has_host_resource(tex),
                "SRV write for texture {i} must not force host allocation"
            );
            textures.push(tex);
        }

        // First GPU use on texture 3 materializes ONLY texture 3 and backfills its slot.
        let mat = host_resource(textures[3]).expect("lazy materialization");
        assert!(has_host_resource(textures[3]));
        assert!(!has_host_resource(textures[0]));
        assert!(crate::memstat::events(crate::memstat::Event::LazyMaterializations) > lazy_before);
        drop(mat);

        for tex in textures {
            assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        }
        assert_eq!(slot::<Release>(sheap, 0x10)(sheap), 0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// A barrier on a deferred texture skips allocating a host texture, while
    /// a later clear + readback materializes it cleanly with valid transitions.
    #[cfg(windows)]
    #[test]
    fn barriers_on_deferred_textures_skip_allocation_until_real_gpu_use() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let mut tex = null;
        let desc = ResourceDesc {
            flags: 1, // ALLOW_RENDER_TARGET
            ..texture(16, 16, 28, 1)
        };
        assert_eq!(
            create_committed(null, &PROPERTIES, 0, &desc, 0, null.cast(), iid, &mut tex),
            S_OK
        );
        assert!(!has_host_resource(tex));

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut alloc, mut cmd) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut alloc),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, alloc, null, &list_iid, &mut cmd),
            S_OK
        );

        #[repr(C)]
        struct TransBarrier {
            kind: u32,
            flags: u32,
            res: This,
            sub_before: u64,
            after: u64,
        }
        let b = TransBarrier {
            kind: 0,
            flags: 0,
            res: tex,
            sub_before: 0xffff_ffff,
            after: 0x4, // RENDER_TARGET
        };
        slot::<extern "system" fn(This, u32, *const TransBarrier)>(cmd, 0xd0)(cmd, 1, &b);
        assert!(
            !has_host_resource(tex),
            "bare barrier must not allocate a host texture"
        );

        let mut rtv_slot = [0u8; 128];
        crate::descriptor::create_rtv(null, tex, std::ptr::null(), rtv_slot.as_mut_ptr() as u64);
        let color = [1.0f32, 0.0, 0.5, 1.0];
        slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const c_void)>(cmd, 0x1a0)(
            cmd,
            rtv_slot.as_ptr() as u64,
            &color,
            0,
            std::ptr::null(),
        );
        assert!(
            has_host_resource(tex),
            "ClearRenderTargetView must materialize the host texture"
        );
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(cmd, 0x48)(cmd),
            S_OK
        );
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &cmd);

        let pixels = read_rgba8(tex, 16, 16);
        assert_eq!(pixels[0], 255);
        assert_eq!(pixels[1], 0);
        assert!((127..=128).contains(&pixels[2]), "got {:?}", &pixels[0..4]);
        assert_eq!(pixels[3], 255);

        assert_eq!(slot::<Release>(cmd, 0x10)(cmd), 0);
        assert_eq!(slot::<Release>(alloc, 0x10)(alloc), 0);
        assert_eq!(slot::<Release>(queue, 0x10)(queue), 0);
        assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Bindless UAV materialization materializes deferred UAV textures on a
    /// shader-visible heap while leaving SRV-only textures deferred.
    #[cfg(windows)]
    #[test]
    fn bindless_uav_flush_materializes_only_uav_textures() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let heap_iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        #[repr(C)]
        struct ExtendedHeap([u32; 4], [u64; 2]);
        let hdesc = ExtendedHeap([0, 16, 1, 0], [0, 0]);
        let mut sheap = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const hdesc.0).cast(),
                &heap_iid,
                &mut sheap
            ),
            S_OK
        );
        let title_heap = crate::descriptor::heap(sheap);
        let _host_heap = title_heap.host().expect("host view heap");
        let start = title_heap.start;

        let backing = vec![0u8; 64 << 10];
        let mut srv_tex = null;
        let mut uav_tex = null;
        let srv_desc = texture(64, 64, 28, 1);
        let uav_desc = ResourceDesc {
            flags: 4, // ALLOW_UNORDERED_ACCESS
            ..texture(64, 64, 28, 1)
        };
        assert_eq!(
            create_at_address(
                null,
                backing.as_ptr() as u64,
                &srv_desc,
                0,
                null.cast(),
                iid,
                &mut srv_tex
            ),
            S_OK
        );
        assert_eq!(
            create_at_address(
                null,
                backing.as_ptr() as u64,
                &uav_desc,
                0,
                null.cast(),
                iid,
                &mut uav_tex
            ),
            S_OK
        );
        crate::descriptor::create_srv(null, srv_tex, std::ptr::null(), start);
        crate::descriptor::create_uav(null, uav_tex, null, std::ptr::null(), start + 32);
        assert!(!has_host_resource(srv_tex));
        assert!(!has_host_resource(uav_tex));

        let list = host_gpu.new_list().expect("host list");
        crate::descriptor::prepare_bindless_heap(&list, sheap as usize);
        assert!(
            has_host_resource(uav_tex),
            "UAV texture materialized for bindless"
        );
        assert!(
            !has_host_resource(srv_tex),
            "all-zero SRV-only texture remains deferred"
        );

        // SAFETY: closing our open test command list before reset.
        let _ = unsafe { list.list.Close() };
        list.reset();
        assert_eq!(slot::<Release>(srv_tex, 0x10)(srv_tex), 0);
        assert_eq!(slot::<Release>(uav_tex, 0x10)(uav_tex), 0);
        assert_eq!(slot::<Release>(sheap, 0x10)(sheap), 0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Bindless (`ResourceDescriptorHeap[]`) preparation materializes and queues
    /// uploads for title-written SRVs whose backing memory is populated (non-zero),
    /// keeps all-zero/dormant SRVs deferred, pins bound textures against eviction
    /// while the bindless list is open, and allows clean eviction once the list
    /// resets.
    #[cfg(windows)]
    #[test]
    fn bindless_deferred_populated_srv_materializes_and_pins_against_eviction() {
        let _guard = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        #[repr(C)]
        struct HeapDescLocal {
            kind: u32,
            count: u32,
            flags: u32,
            node_mask: u32,
        }
        const HEAP_IID: Iid = crate::object::guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        let mut sheap = std::ptr::null();
        let hdesc = HeapDescLocal {
            kind: 0,
            count: 32,
            flags: 1,
            node_mask: 0,
        };
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                std::ptr::from_ref(&hdesc).cast(),
                &HEAP_IID,
                &mut sheap,
            ),
            S_OK
        );
        let title_heap = crate::descriptor::heap(sheap);
        let _ = title_heap.host();
        let start = title_heap.start;

        let zero_backing = vec![0u8; 64 << 10];
        let mut populated_backing = vec![0u8; 64 << 10];
        populated_backing[4096..4100].copy_from_slice(&[0xff, 0x80, 0x40, 0xff]);
        let mut d = texture(64, 64, 28, 1);
        d.layout = 1; // LINEAR

        let (mut zero_tex, mut pop_tex) = (std::ptr::null(), std::ptr::null());
        assert_eq!(
            create_at_address(
                null,
                zero_backing.as_ptr() as u64,
                &d,
                0,
                null.cast(),
                iid,
                &mut zero_tex
            ),
            S_OK
        );
        assert_eq!(
            create_at_address(
                null,
                populated_backing.as_ptr() as u64,
                &d,
                0,
                null.cast(),
                iid,
                &mut pop_tex
            ),
            S_OK
        );
        crate::descriptor::create_srv(null, zero_tex, std::ptr::null(), start);
        crate::descriptor::create_srv(null, pop_tex, std::ptr::null(), start + 32);
        assert!(!has_host_resource(zero_tex));
        assert!(!has_host_resource(pop_tex));

        let list = host_gpu.new_list().expect("host list");
        crate::descriptor::prepare_bindless_heap(&list, sheap as usize);
        assert!(
            !has_host_resource(zero_tex),
            "unpopulated title-written texture stays deferred"
        );
        assert!(
            has_host_resource(pop_tex),
            "populated title-written texture materializes for bindless SRV"
        );
        assert!(
            crate::descriptor::is_texture_pinned_by_bindless(pop_tex as usize),
            "open bindless list pins shader-visible heap textures against eviction"
        );

        // Eviction must refuse to evict while the bindless list is still open.
        let _ = evict_idle_textures(0);
        assert!(
            has_host_resource(pop_tex),
            "eviction must not touch a texture pinned by an open bindless list"
        );

        // Execute the list (without resetting yet): once the GPU finishes, both
        // `list_guard` and `bindless_guard` must release immediately so idle
        // textures can be evicted before the title calls `Reset()` frames later.
        // SAFETY: open list.
        unsafe { list.list.Close().unwrap() };
        host_gpu.execute(&[&list]);
        assert!(
            !crate::descriptor::is_texture_pinned_by_bindless(pop_tex as usize),
            "completed list must unpin bindless heap textures before Reset()"
        );

        let freed = evict_idle_textures(0);
        assert!(
            freed > 0,
            "idle texture must evict right after execute() even before list.reset()"
        );
        assert!(!has_host_resource(pop_tex));

        list.reset();

        assert_eq!(slot::<Release>(zero_tex, 0x10)(zero_tex), 0);
        assert_eq!(slot::<Release>(pop_tex, 0x10)(pop_tex), 0);
        assert_eq!(slot::<Release>(sheap, 0x10)(sheap), 0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Concurrent `texture_at` / shader-visible `HostHeap::mirror` and
    /// `evict_idle_textures` / `on_title_memory_remapped` must never deadlock
    /// (verifies acyclic lock order between `TITLE_WRITTEN_TEXTURES`,
    /// `PLACED_TEXTURES`, `SHADER_HEAPS`, `HostHeap.made`, and `Resource.host`).
    #[cfg(windows)]
    #[test]
    fn concurrent_placed_texture_mirror_and_eviction_never_deadlocks() {
        let _guard = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let backing = vec![0x5Au8; 256 << 10];
        let base_addr = backing.as_ptr() as u64;
        let d = texture(64, 64, 28, 1);
        // SAFETY: `ResourceDesc` is 56 bytes = 14 u32 words.
        let words: [u32; 14] =
            unsafe { std::ptr::from_ref(&d).cast::<[u32; 14]>().read_unaligned() };

        let t1 = std::thread::spawn(move || {
            for i in 0..40u64 {
                let addr = base_addr + (i % 4) * 16384;
                let _ = texture_at(addr, &words);
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..40u64 {
                let _ = evict_idle_textures(0);
                let addr = base_addr + (i % 4) * 16384;
                on_title_memory_remapped(addr, 16384);
            }
        });
        t1.join().expect("texture_at thread finished");
        t2.join().expect("eviction thread finished");
        on_title_memory_remapped(base_addr, backing.len() as u64);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Regression for Test #16 (`nvwgf2umx.dll` crash in `make_at -> make_owned
    /// -> placed_texture`): multiple worker threads building SRVs for `View::Placed`
    /// textures concurrently with aggressive `evict_idle_textures(0)` and
    /// `on_title_memory_remapped` must never use-after-free a placed `Resource`.
    #[cfg(windows)]
    #[test]
    fn concurrent_placed_texture_make_at_and_reclaim_never_uaf() {
        let _guard = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let heap = std::sync::Arc::new(
            crate::descriptor::host::HostHeap::new(false, 16, 0).expect("host view heap"),
        );
        let backing = vec![0xA5u8; 256 << 10];
        let base_addr = backing.as_ptr() as u64;
        let d = texture(64, 64, 28, 1);
        // SAFETY: `ResourceDesc` is 56 bytes = 14 u32 words.
        let words: [u32; 14] =
            unsafe { std::ptr::from_ref(&d).cast::<[u32; 14]>().read_unaligned() };

        let mut workers = Vec::new();
        for tid in 0..4u32 {
            let heap = heap.clone();
            workers.push(std::thread::spawn(move || {
                for i in 0..50u64 {
                    let addr = base_addr + ((i + u64::from(tid)) % 4) * 16384;
                    let mut srv = [0u32; 16];
                    srv[0] = 28; // DXGI_FORMAT_R8G8B8A8_UNORM
                    srv[1] = 4; // D3D12_SRV_DIMENSION_TEXTURE2D
                    srv[2] = 0x1688; // D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
                    srv[4] = 0; // MostDetailedMip = 0 (after union alignment padding)
                    srv[5] = 1; // MipLevels = 1
                    srv[10] = addr as u32;
                    srv[11] = (addr >> 32) as u32;
                    let view = crate::descriptor::View::Placed(false, words, srv);
                    let slot = heap.count() + tid;
                    let bound = heap.make_at(slot, &view);
                    assert!(bound.is_some(), "placed texture SRV creation must succeed");
                }
            }));
        }
        let evictor = std::thread::spawn(move || {
            for i in 0..80u64 {
                let _ = evict_idle_textures(0);
                let addr = base_addr + (i % 4) * 16384;
                on_title_memory_remapped(addr, 16384);
            }
        });
        for w in workers {
            w.join().expect("worker thread finished");
        }
        evictor.join().expect("evictor thread finished");
        drop(heap);
        on_title_memory_remapped(base_addr, backing.len() as u64);
        let _ = evict_idle_textures(0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// `PLACED_TEXTURES` entries are evicted and freed when the title remaps
    /// their backing address range.
    #[cfg(windows)]
    #[test]
    fn placed_textures_evict_on_title_memory_remap() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let backing = vec![0u8; 256 << 10];
        let addr = backing.as_ptr() as u64;
        let d = texture(64, 64, 28, 1);
        // SAFETY: `ResourceDesc` is 56 bytes = 14 u32 words.
        let words: [u32; 14] =
            unsafe { std::ptr::from_ref(&d).cast::<[u32; 14]>().read_unaligned() };
        let evicted_before = crate::memstat::events(crate::memstat::Event::TexturesEvicted);

        let res = texture_at(addr, &words).expect("placed texture");
        assert!(has_host_resource(res.resource()));
        drop(res);
        assert!(
            PLACED_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&(addr, words))
        );

        crate::title_memory_remapped(addr as usize, backing.len());
        assert!(
            !PLACED_TEXTURES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&(addr, words)),
            "remapping must remove overlapping placed textures"
        );
        assert!(crate::memstat::events(crate::memstat::Event::TexturesEvicted) > evicted_before);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Copies `texels` (one 32-bit texel each) into mip 0 of `image`'s host
    /// texture through an upload buffer, as a host GPU write would leave it.
    #[cfg(windows)]
    fn write_host_r32(image: This, width: u32, height: u32, format: u32, texels: &[u32]) {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::Interface;
        let host = crate::host::host().expect("host");
        let pitch = (width * 4).next_multiple_of(256);
        let len = u64::from(pitch * height);
        let upload_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut upload: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of an upload buffer, then a mapped write of
        // `height` rows of `width` texels inside its `len` bytes.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &upload_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut upload,
                )
                .unwrap();
        }
        let upload = upload.unwrap();
        let mut mapped = std::ptr::null_mut();
        // SAFETY: as above.
        unsafe {
            upload.Map(0, None, Some(&mut mapped)).unwrap();
            let base = mapped.cast::<u8>();
            for y in 0..height as usize {
                let row = &texels[y * width as usize..][..width as usize];
                std::ptr::copy_nonoverlapping(
                    row.as_ptr().cast::<u8>(),
                    base.add(y * pitch as usize),
                    width as usize * 4,
                );
            }
            upload.Unmap(0, None);
        }
        let host_image = host_resource(image).unwrap();
        let list = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x400).unwrap();
        #[repr(C)]
        struct Location {
            resource: *const c_void,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format,
            width,
            height,
            depth: 1,
            row_pitch: pitch,
        };
        let to = Location {
            resource: host_image.as_raw().cast_const(),
            kind: 0,
            footprint: rows,
        };
        let from = Location {
            resource: upload.as_raw().cast_const(),
            kind: 1,
            footprint: rows,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            if before != 0x400 {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &host_image,
                    D3D12_RESOURCE_STATES(before as i32),
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            list.list.CopyTextureRegion(
                (&raw const to).cast(),
                0,
                0,
                0,
                (&raw const from).cast(),
                None,
            );
        }
        list.close();
        host.execute(&[&list]);
    }

    /// Reads mip 0 of `image`'s host texture back as 32-bit texels.
    #[cfg(windows)]
    fn read_host_r32(image: This, width: u32, height: u32, format: u32) -> Vec<u32> {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::Interface;
        let host = crate::host::host().expect("host");
        let pitch = (width * 4).next_multiple_of(256);
        let len = u64::from(pitch * height);
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let list = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        #[repr(C)]
        struct Location {
            resource: *const c_void,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format,
            width,
            height,
            depth: 1,
            row_pitch: pitch,
        };
        let to = Location {
            resource: readback.as_raw().cast_const(),
            kind: 1,
            footprint: rows,
        };
        let from = Location {
            resource: host_image.as_raw().cast_const(),
            kind: 0,
            footprint: rows,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            if before != 0x800 {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &host_image,
                    D3D12_RESOURCE_STATES(before as i32),
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            list.list.CopyTextureRegion(
                (&raw const to).cast(),
                0,
                0,
                0,
                (&raw const from).cast(),
                None,
            );
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is `len` bytes.
        unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), len as usize);
            (0..height as usize)
                .flat_map(|y| {
                    bytes[y * pitch as usize..][..width as usize * 4]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes(*c))
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    }

    /// Wave A 1B regression (behaviour identified by fxceless/victory-gta6;
    /// independently written). Two placed textures over the same title memory
    /// in different formats: the host GPU writes the one with unordered
    /// access (`R32_UINT`, standing in for the title's compute pass), and a
    /// shader then reads the other (`R32_FLOAT`) as a draw would make it ready
    /// (`draw.rs` `readable`: `texture::upload` for a title-written texture).
    /// The reader must see the bits the GPU wrote.
    ///
    /// Today each `(address, desc)` gets its own host texture and the reader
    /// is refreshed from title memory, which the GPU write never reached, so
    /// it reads the old memory (zeros) instead. WARP only
    /// (`VICTORY_D3D12_WARP=1`). Ignored until the 1B fix lands; run it with
    /// `VICTORY_D3D12_WARP=1 VICTORY_D3D12_DEBUG=1 cargo test -p victory-gpu
    /// --features diagnostics -- --ignored --exact --test-threads=1
    /// resource::tests::wave_a_1b_format_aliased_placed_texture_sees_gpu_writes`.
    #[cfg(windows)]
    #[test]
    #[ignore = "Wave A 1B: known defect (format-aliased placed textures do not share GPU-written contents)"]
    fn wave_a_1b_format_aliased_placed_texture_sees_gpu_writes() {
        let _serial = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if std::env::var_os("VICTORY_D3D12_WARP").is_none() {
            eprintln!("Wave A 1B regression runs on WARP only (VICTORY_D3D12_WARP=1); skipped");
            return;
        }
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        const R32_FLOAT: u32 = 41;
        const R32_UINT: u32 = 42;
        let (width, height) = (64u32, 64u32);
        // Title memory for both views, 64 KiB aligned, never written by the title.
        let backing = vec![0u8; 2 << 20];
        let addr = (backing.as_ptr() as u64).next_multiple_of(64 << 10);
        let words = |d: &ResourceDesc| -> [u32; 14] {
            // SAFETY: `ResourceDesc` is 56 bytes = 14 u32 words.
            unsafe { std::ptr::from_ref(d).cast::<[u32; 14]>().read_unaligned() }
        };
        let writer_desc = ResourceDesc {
            flags: 4, // ALLOW_UNORDERED_ACCESS: written by the GPU
            ..texture(u64::from(width), height, R32_UINT, 1)
        };
        let reader_desc = texture(u64::from(width), height, R32_FLOAT, 1);

        let writer = texture_at(addr, &words(&writer_desc)).expect("writer view");
        let writer_host = host_resource(writer.resource()).expect("writer host texture");
        assert!(
            !crate::texture::title_written(&writer_host),
            "the UAV view is GPU-written"
        );
        let want: Vec<u32> = (0..width * height)
            .map(|i| 0.5f32.to_bits().wrapping_add(i))
            .collect();
        write_host_r32(writer.resource(), width, height, R32_UINT, &want);
        assert_eq!(
            read_host_r32(writer.resource(), width, height, R32_UINT),
            want,
            "the GPU write landed in the writer's host texture"
        );

        let reader = texture_at(addr, &words(&reader_desc)).expect("reader view");
        let reader_host = host_resource(reader.resource()).expect("reader host texture");
        // As `draw.rs` `readable` makes a sampled view ready.
        let list = host_gpu.new_list().unwrap();
        if crate::texture::title_written(&reader_host) {
            crate::texture::upload(&list, reader.resource(), &reader_host);
        }
        crate::list::ensure_state(&list, reader.resource(), 0x40 | 0x80);
        list.close();
        host_gpu.execute(&[&list]);
        let got = read_host_r32(reader.resource(), width, height, R32_FLOAT);
        crate::host::assert_no_debug_errors(&host_gpu.device);
        let differing = got.iter().zip(&want).filter(|(g, w)| g != w).count();
        eprintln!(
            "1B warp: reader texels differing from the GPU write: {differing}/{}; first got {:08x?}, want {:08x?}",
            want.len(),
            &got[..4],
            &want[..4]
        );
        assert_eq!(
            differing, 0,
            "the R32_FLOAT view of the same memory does not see the R32_UINT view's GPU write"
        );
        drop(reader);
        drop(writer);
        crate::title_memory_remapped(addr as usize, 1 << 20);
        drop(backing);
    }

    /// Idle title-written host textures are evicted under commit pressure and
    /// re-materialize + re-upload identical pixels on next use, while RT/UAV
    /// textures and textures referenced by an open list are never evicted.
    #[cfg(windows)]
    #[test]
    fn idle_title_written_textures_evict_and_reupload_cleanly() {
        let _serial = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let (width, height) = (64u32, 32u32);
        let mut title_tex = null;
        let desc = texture(u64::from(width), height, 28, 1);
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                0,
                null.cast(),
                iid,
                &mut title_tex
            ),
            S_OK
        );
        let mut rt_tex = null;
        let rt_desc = ResourceDesc {
            flags: 1, // ALLOW_RENDER_TARGET
            ..desc
        };
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &rt_desc,
                0,
                null.cast(),
                iid,
                &mut rt_tex
            ),
            S_OK
        );
        let _ = host_resource(rt_tex).expect("rt materialized");

        // Fill tiled title memory and upload once.
        let (memory, d) = texture_memory(title_tex).unwrap();
        let layout = texture_layout(&d).unwrap().planes[0].layout.clone();
        let expected: Vec<u8> = (0..width * height * 4)
            .map(|i| (i as u8).wrapping_mul(17).wrapping_add(5))
            .collect();
        // SAFETY: `memory` holds `layout.size` bytes.
        let tiled =
            unsafe { std::slice::from_raw_parts_mut(memory.cast_mut(), layout.size as usize) };
        let pitch = width as usize * 4;
        layout
            .tile(tiled, 0, 0, &expected, pitch, pitch * height as usize)
            .unwrap();

        let upload_once = |image: This| {
            let list = host_gpu.new_list().unwrap();
            let ht = host_resource(image).unwrap();
            crate::texture::upload(&list, image, &ht);
            crate::list::ensure_state(&list, image, 0x40 | 0x80);
            list.close();
            host_gpu.execute(&[&list]);
        };
        upload_once(title_tex);
        assert_eq!(read_rgba8(title_tex, width, height), expected);

        // Hold an open list referencing `title_tex` -> must NOT be evicted.
        let open_list = host_gpu.new_list().unwrap();
        crate::list::ensure_state(&open_list, title_tex, 0x40 | 0x80);
        let _ = evict_idle_textures(0);
        assert!(
            has_host_resource(title_tex),
            "texture held by an open command list must not be evicted"
        );
        assert!(
            has_host_resource(rt_tex),
            "render target texture must never be evicted"
        );
        open_list.close();
        open_list.reset();
        drop(open_list);

        // Now idle -> eviction drops the host texture, freeing commit.
        let freed = evict_idle_textures(0);
        assert!(freed > 0);
        assert!(
            !has_host_resource(title_tex),
            "idle title-written texture must be evicted under pressure"
        );
        assert!(
            has_host_resource(rt_tex),
            "render target texture still protected"
        );

        // Next use re-materializes and re-uploads exact pixels cleanly.
        upload_once(title_tex);
        assert!(has_host_resource(title_tex));
        assert_eq!(
            read_rgba8(title_tex, width, height),
            expected,
            "re-uploaded pixels after eviction must match"
        );

        assert_eq!(slot::<Release>(title_tex, 0x10)(title_tex), 0);
        assert_eq!(slot::<Release>(rt_tex, 0x10)(rt_tex), 0);
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Creating 1,250 synthetic overlapping/aliased textures (mirroring GTA VI's
    /// 1,210 startup texture burst) plus 1,250 shader-visible SRVs allocates
    /// zero host D3D12 textures until actually bound, materializes only the 8
    /// used textures, and returns cleanly to baseline on release.
    #[cfg(windows)]
    #[test]
    fn thousands_of_deferred_textures_consume_zero_host_vram_and_release_cleanly() {
        let _serial = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let heap_iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        const COUNT: usize = 1250;
        #[repr(C)]
        struct ExtendedHeap([u32; 4], [u64; 2]);
        let hdesc = ExtendedHeap([0, COUNT as u32 + 16, 1, 0], [0, 0]);
        let mut sheap = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const hdesc.0).cast(),
                &heap_iid,
                &mut sheap
            ),
            S_OK
        );
        let title_heap = crate::descriptor::heap(sheap);
        let _host_heap = title_heap.host().expect("host view heap");
        let start = title_heap.start;

        let made_before = crate::memstat::made(crate::memstat::Kind::HostTexture);
        let def_before = crate::memstat::live(crate::memstat::Kind::DeferredTexture).1;
        // Overlapping/aliased virtual backing for 1,250 1024x1024 RGBA8 textures (4 MiB each = 5 GiB nominal).
        let backing = vec![0u8; 64 << 10];
        let mut textures = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            let mut tex = null;
            let desc = ResourceDesc {
                flags: if i % 3 == 0 { 1 } else { 0 }, // mix of RT and SRV-only
                ..texture(1024, 1024, 28, 1)
            };
            assert_eq!(
                create_at_address(
                    null,
                    backing.as_ptr() as u64,
                    &desc,
                    0,
                    null.cast(),
                    iid,
                    &mut tex
                ),
                S_OK
            );
            crate::descriptor::create_srv(null, tex, std::ptr::null(), start + (i as u64) * 32);
            textures.push(tex);
        }
        assert_eq!(
            crate::memstat::made(crate::memstat::Kind::HostTexture),
            made_before,
            "1,250 unused textures + SRVs must allocate 0 host D3D12 textures"
        );
        assert!(
            crate::memstat::live(crate::memstat::Kind::DeferredTexture).1
                >= def_before + COUNT as u64
        );

        // Delayed first use on 8 of the 1,250 textures.
        for idx in (0..COUNT).step_by(COUNT / 8).take(8) {
            let ht = host_resource(textures[idx]).expect("delayed materialization");
            assert!(has_host_resource(textures[idx]));
            drop(ht);
        }
        assert_eq!(
            crate::memstat::made(crate::memstat::Kind::HostTexture) - made_before,
            8,
            "only the 8 used textures materialized"
        );

        for tex in textures {
            assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        }
        assert_eq!(slot::<Release>(sheap, 0x10)(sheap), 0);
        assert_eq!(
            crate::memstat::live(crate::memstat::Kind::DeferredTexture).1,
            def_before,
            "deferred count returns to baseline after release"
        );
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Long-running stability: 20 cycles of create, upload, evict under
    /// pressure, re-upload, and release keep live host/deferred texture counts
    /// and bytes flat with zero D3D12 debug errors.
    #[cfg(windows)]
    #[test]
    fn long_running_texture_alloc_evict_release_cycles_stay_bounded() {
        let _serial = TEXTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let (host_b0, host_c0, _) = crate::memstat::live(crate::memstat::Kind::HostTexture);
        let (def_b0, def_c0, _) = crate::memstat::live(crate::memstat::Kind::DeferredTexture);

        for cycle in 0..20u8 {
            let mut batch = Vec::new();
            for j in 0..4u8 {
                let mut tex = null;
                let desc = texture(64, 64, 28, 1);
                assert_eq!(
                    create_committed(null, &PROPERTIES, 0, &desc, 0, null.cast(), iid, &mut tex),
                    S_OK
                );
                let (mem, d) = texture_memory(tex).unwrap();
                let layout = texture_layout(&d).unwrap().planes[0].layout.clone();
                // SAFETY: `mem` holds `layout.size` bytes.
                unsafe {
                    mem.cast_mut()
                        .write_bytes(cycle.wrapping_add(j + 1), layout.size as usize)
                };
                let list = host_gpu.new_list().unwrap();
                let ht = host_resource(tex).unwrap();
                crate::texture::upload(&list, tex, &ht);
                crate::list::ensure_state(&list, tex, 0x40 | 0x80);
                list.close();
                host_gpu.execute(&[&list]);
                batch.push(tex);
            }
            let freed = evict_idle_textures(0);
            assert!(
                freed > 0,
                "cycle {cycle} evicted idle title-written textures"
            );
            for &tex in &batch {
                assert!(!has_host_resource(tex));
            }
            for tex in batch {
                assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
            }
        }

        let (host_b1, host_c1, _) = crate::memstat::live(crate::memstat::Kind::HostTexture);
        let (def_b1, def_c1, _) = crate::memstat::live(crate::memstat::Kind::DeferredTexture);
        assert_eq!(
            (host_b1, host_c1),
            (host_b0, host_c0),
            "host texture counts flat"
        );
        assert_eq!(
            (def_b1, def_c1),
            (def_b0, def_c0),
            "deferred texture counts flat"
        );
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Probes `ID3D12Device3::OpenExistingHeapFromAddress` support and
    /// constraints on plain `VirtualAlloc`, `MEM_WRITE_WATCH`, and `MEM_MAPPED`
    /// memory (Phase C architectural evaluation). Uses a standalone D3D12
    /// device so negative capability probes never pollute the shared host
    /// debug InfoQueue.
    #[cfg(windows)]
    #[test]
    fn existing_heap_from_address_capability_probe() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::Interface;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
            MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mut probe_dev: Option<ID3D12Device> = None;
        // SAFETY: standalone D3D12 device for isolated capability probing.
        if unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut probe_dev) }.is_err() {
            return;
        }
        let Some(probe_dev) = probe_dev else {
            return;
        };
        let Ok(dev3) = probe_dev.cast::<ID3D12Device3>() else {
            return;
        };
        let check = |ptr: *const c_void| -> (bool, bool, bool) {
            // SAFETY: `ptr` is a live 64 KiB aligned allocation for the duration of the check.
            let heap: windows::core::Result<ID3D12Heap> =
                unsafe { dev3.OpenExistingHeapFromAddress(ptr) };
            let Ok(heap) = heap else {
                return (false, false, false);
            };
            // SAFETY: live heap query.
            let hdesc = unsafe { heap.GetDesc() };
            let base_flags = if hdesc.Flags.0 & D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER.0 != 0 {
                D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER
            } else {
                D3D12_RESOURCE_FLAG_NONE
            };
            let make_desc = |flags: D3D12_RESOURCE_FLAGS| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: KIB_64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: base_flags | flags,
                ..Default::default()
            };
            let init_state = if hdesc.Properties.Type == D3D12_HEAP_TYPE_UPLOAD {
                D3D12_RESOURCE_STATE_GENERIC_READ
            } else if hdesc.Properties.Type == D3D12_HEAP_TYPE_READBACK {
                D3D12_RESOURCE_STATE_COPY_DEST
            } else {
                D3D12_RESOURCE_STATE_COMMON
            };
            let ro_desc = make_desc(D3D12_RESOURCE_FLAG_NONE);
            let mut ro_res: Option<ID3D12Resource> = None;
            // SAFETY: read-only placed buffer inside the opened 64 KiB heap.
            let ro_ok = unsafe {
                probe_dev.CreatePlacedResource(&heap, 0, &ro_desc, init_state, None, &mut ro_res)
            }
            .is_ok();
            drop(ro_res);
            let uav_desc = make_desc(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
            let mut uav_res: Option<ID3D12Resource> = None;
            // SAFETY: UAV placed buffer inside the opened 64 KiB heap.
            let uav_ok = unsafe {
                probe_dev.CreatePlacedResource(&heap, 0, &uav_desc, init_state, None, &mut uav_res)
            }
            .is_ok();
            drop(uav_res);
            drop(heap);
            (true, ro_ok, uav_ok)
        };

        // SAFETY: 64 KiB plain VirtualAlloc freed after heap drop.
        let plain = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                KIB_64 as usize,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!plain.is_null());
        let plain_res = check(plain);
        // SAFETY: heap already dropped above.
        unsafe { VirtualFree(plain, 0, MEM_RELEASE) };

        // SAFETY: 64 KiB write-watched VirtualAlloc freed after heap drop.
        let watched = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                KIB_64 as usize,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        assert!(!watched.is_null());
        let watched_res = check(watched);
        // SAFETY: heap already dropped above.
        unsafe { VirtualFree(watched, 0, MEM_RELEASE) };

        // SAFETY: 64 KiB pagefile section view unmapped after heap drop.
        let (section, mapped) = unsafe {
            let s = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                KIB_64 as u32,
                std::ptr::null(),
            );
            assert!(!s.is_null());
            let v = MapViewOfFile(s, FILE_MAP_WRITE, 0, 0, KIB_64 as usize);
            (s, v)
        };
        assert!(!mapped.Value.is_null());
        let mapped_res = check(mapped.Value);
        // SAFETY: heap already dropped above.
        unsafe {
            UnmapViewOfFile(mapped);
            windows_sys::Win32::Foundation::CloseHandle(section);
        };

        println!(
            "OpenExistingHeapFromAddress probe (adapter={}, existing_heaps={}): (opened, ro_buf, uav_buf) plain={plain_res:?}, write_watch={watched_res:?}, mapped_section={mapped_res:?}",
            host_gpu.caps.adapter_name, host_gpu.caps.existing_heaps
        );
        if !host_gpu.caps.existing_heaps {
            assert!(!plain_res.0);
        }
    }

    /// Synthetic Phase 12 before/after benchmark (`--ignored --test-threads=1 --nocapture`):
    /// compares eager vs lazy host texture creation on 256 x 4 MiB (1 GiB)
    /// aliased textures (16 used) and measures eviction reclamation across
    /// 64 x 4 MiB (256 MiB) title-written textures + 10 steady-state cycles.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn bench_phase12_lazy_textures_and_eviction() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let mib = |b: i128| b as f64 / (1 << 20) as f64;
        let commit = || crate::memstat::system_commit().expect("commit").0 as i128;
        let private = || crate::memstat::process().expect("process").0 as i128;
        let local = || host_gpu.video_memory().map_or(0, |v| v[0].0 as i128);

        // 1. BEFORE baseline: 256 x 4 MiB (1024x1024 RGBA8) eager D3D12 DEFAULT textures.
        let (c0, p0, l0) = (commit(), private(), local());
        let eager_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 1024,
            Height: 1024,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let eager: Vec<ID3D12Resource> = (0..256)
            .map(|_| {
                let mut t: Option<ID3D12Resource> = None;
                // SAFETY: plain texture creation.
                unsafe {
                    host_gpu
                        .device
                        .CreateCommittedResource(
                            &heap,
                            D3D12_HEAP_FLAG_NONE,
                            &eager_desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut t,
                        )
                        .unwrap();
                }
                t.unwrap()
            })
            .collect();
        let (c_eager, p_eager, l_eager) = (commit() - c0, private() - p0, local() - l0);
        drop(eager);
        // Settle driver cache after eager release.
        let settle = host_gpu.new_list().unwrap();
        settle.close();
        host_gpu.execute(&[&settle]);
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 2. AFTER (Phase 12 lazy): 256 x 4 MiB aliased textures + 256 SRVs, 0 used then 16 used.
        let (null, iid) = (std::ptr::null(), &RESOURCE_IIDS[0]);
        let heap_iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        #[repr(C)]
        struct ExtendedHeap([u32; 4], [u64; 2]);
        let hdesc = ExtendedHeap([0, 512, 1, 0], [0, 0]);
        let mut sheap = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const hdesc.0).cast(),
                &heap_iid,
                &mut sheap
            ),
            S_OK
        );
        let title_heap = crate::descriptor::heap(sheap);
        let _host_heap = title_heap.host().expect("host view heap");
        let start = title_heap.start;

        let (c1, p1, l1) = (commit(), private(), local());
        let backing = vec![0u8; 64 << 10];
        let mut lazy_tex = Vec::with_capacity(256);
        for i in 0..256u64 {
            let mut tex = null;
            let desc = ResourceDesc {
                flags: 1,
                ..texture(1024, 1024, 28, 1)
            };
            assert_eq!(
                create_at_address(
                    null,
                    backing.as_ptr() as u64,
                    &desc,
                    0,
                    null.cast(),
                    iid,
                    &mut tex
                ),
                S_OK
            );
            crate::descriptor::create_srv(null, tex, std::ptr::null(), start + i * 32);
            lazy_tex.push(tex);
        }
        let (c_lazy0, p_lazy0, l_lazy0) = (commit() - c1, private() - p1, local() - l1);
        let (ht_live0, ht_cnt0, _) = crate::memstat::live(crate::memstat::Kind::HostTexture);
        let (df_live0, df_cnt0, _) = crate::memstat::live(crate::memstat::Kind::DeferredTexture);

        // Materialize 16 of 256 (64 MiB active working set).
        for &tex in lazy_tex.iter().take(16) {
            let _ = host_resource(tex).expect("materialize");
        }
        let (c_lazy16, p_lazy16, l_lazy16) = (commit() - c1, private() - p1, local() - l1);
        let (ht_live16, ht_cnt16, _) = crate::memstat::live(crate::memstat::Kind::HostTexture);
        for tex in lazy_tex {
            assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        }
        assert_eq!(slot::<Release>(sheap, 0x10)(sheap), 0);

        // 3. Idle title-written texture eviction: 64 x 4 MiB = 256 MiB materialized -> evicted.
        let mut evict_tex = Vec::with_capacity(64);
        for _ in 0..64 {
            let mut tex = null;
            let desc = texture(1024, 1024, 28, 1);
            assert_eq!(
                create_at_address(
                    null,
                    backing.as_ptr() as u64,
                    &desc,
                    0,
                    null.cast(),
                    iid,
                    &mut tex
                ),
                S_OK
            );
            let _ = host_resource(tex).expect("materialize title-written");
            evict_tex.push(tex);
        }
        let ht_before_evict = crate::memstat::live(crate::memstat::Kind::HostTexture);
        let freed = evict_idle_textures(0);
        let ht_after_evict = crate::memstat::live(crate::memstat::Kind::HostTexture);
        for tex in evict_tex {
            assert_eq!(slot::<Release>(tex, 0x10)(tex), 0);
        }

        let line1 = format!(
            "phase12-lazy-256x4MiB: BEFORE(eager 256) commit={:+.0}MiB private={:+.0}MiB gpu-local={:+.0}MiB | AFTER(lazy 0/256 used) commit={:+.0}MiB private={:+.0}MiB gpu-local={:+.0}MiB host-tex={}({:.0}MiB) deferred={}({:.0}MiB) | AFTER(lazy 16/256 used) commit={:+.0}MiB private={:+.0}MiB gpu-local={:+.0}MiB host-tex={}({:.0}MiB)",
            mib(c_eager),
            mib(p_eager),
            mib(l_eager),
            mib(c_lazy0),
            mib(p_lazy0),
            mib(l_lazy0),
            ht_cnt0,
            mib(ht_live0 as i128),
            df_cnt0,
            mib(df_live0 as i128),
            mib(c_lazy16),
            mib(p_lazy16),
            mib(l_lazy16),
            ht_cnt16,
            mib(ht_live16 as i128),
        );
        let line2 = format!(
            "phase12-evict-64x4MiB: before-evict host-tex={}({:.0}MiB) -> freed={:.0}MiB -> after-evict host-tex={}({:.0}MiB)",
            ht_before_evict.1,
            mib(ht_before_evict.0 as i128),
            mib(freed as i128),
            ht_after_evict.1,
            mib(ht_after_evict.0 as i128),
        );
        println!("{line1}");
        println!("{line2}");
        crate::memstat::report(&format!("synthetic {line1}"));
        crate::memstat::report(&format!("synthetic {line2}"));
        assert_eq!(ht_cnt0, 0);
        assert_eq!(ht_cnt16, 16);
        assert!(freed >= 256 << 20);
        assert_eq!(ht_after_evict.1, 0);
    }

    /// A depth target the title also writes as unordered access (shadow
    /// maps): what a compute shader writes through its view reaches the
    /// depth, in the list and in the next one, and the depth keeps its
    /// values where the shader does not write.
    #[cfg(windows)]
    #[test]
    fn unordered_writes_reach_a_depth_target() {
        use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::core::{Interface, s};
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null: This = std::ptr::null();
        let mut depth = null;
        // 8x8 D16_UNORM, depth and unordered access.
        let desc = ResourceDesc {
            flags: 2 | 4,
            ..texture(8, 8, 55, 1)
        };
        const DEPTH_WRITE: u32 = 0x10;
        let (iid, clear) = (&RESOURCE_IIDS[0], null.cast());
        let made = create_committed(
            null,
            &PROPERTIES,
            0,
            &desc,
            DEPTH_WRITE,
            clear,
            iid,
            &mut depth,
        );
        assert_eq!(made, S_OK);
        // The title's view, R16_UNORM 2D.
        let heap = crate::descriptor::host::HostHeap::new(false, 1, 0).unwrap();
        let view = Some([56, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        heap.make_at(
            0,
            &crate::descriptor::View::Unordered(depth as usize, 0, view),
        );
        const FILL: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), DescriptorTable(UAV(u0))"
cbuffer Fill : register(b0) { float value; uint columns; };
RWTexture2D<unorm float> depth : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x < columns) depth[id.xy] = value;
}
"#;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; out pointer
        // ours. Then the blob's own bytes, the root signature inside.
        let (root, pso) = unsafe {
            let (source, len) = (FILL.as_ptr().cast(), FILL.len());
            let (name, main, model) = (s!("fill"), s!("main"), s!("cs_5_1"));
            D3DCompile(
                source, len, name, None, None, main, model, 0, 0, &mut code, None,
            )
            .unwrap();
            let code = code.unwrap();
            let (start, size) = (code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let bytes = std::slice::from_raw_parts(start, size);
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            };
            let pso: ID3D12PipelineState = host.device.CreateComputePipelineState(&desc).unwrap();
            (root, pso)
        };
        // Writes `value` to the columns before `columns`.
        let fill = |list: &crate::host::HostList, value: f32, columns: u32| {
            // As draws ready the views their shaders write.
            const UNORDERED_ACCESS: u32 = 0x8;
            crate::list::ensure_state(list, depth, UNORDERED_ACCESS);
            let constants = [value.to_bits(), columns];
            // SAFETY: an open list; our heap, root signature and pipeline.
            unsafe {
                let l = &list.list;
                l.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                l.SetComputeRootSignature(&root);
                l.SetPipelineState(&pso);
                l.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                l.SetComputeRootDescriptorTable(1, heap.heap.GetGPUDescriptorHandleForHeapStart());
                l.Dispatch(1, 1, 1);
            }
        };

        // One list clears the depth, then writes four columns as it ends.
        let first = host.new_list().unwrap();
        crate::list::ensure_state(&first, depth, DEPTH_WRITE);
        let dsv = D3D12_DEPTH_STENCIL_VIEW_DESC {
            Format: DXGI_FORMAT_D16_UNORM,
            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let depth_res = host_resource(depth).unwrap();
        let dsv = host.dsv(&depth_res, Some(&dsv));
        // SAFETY: an open list and a fresh view.
        unsafe {
            first
                .list
                .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.25, 0, None)
        };
        fill(&first, 0.75, 4);
        first.close();
        host.execute(&[&first]);

        // The next writes two columns, then copies the depth out.
        let second = host.new_list().unwrap();
        fill(&second, 0.5, 2);
        crate::list::ensure_state(&second, depth, 0x800); // copy source
        let memory = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let buffer = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 8 * 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        let state = D3D12_RESOURCE_STATE_COPY_DEST;
        let flags = D3D12_HEAP_FLAG_NONE;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            let device = &host.device;
            device
                .CreateCommittedResource(&memory, flags, &buffer, state, None, &mut out)
                .unwrap();
        }
        let readback = out.unwrap();
        #[repr(C)]
        struct Location(This, u32, PlacedFootprint);
        // As the host has the depth: R16_TYPELESS.
        let rows = PlacedFootprint {
            offset: 0,
            format: 53,
            width: 8,
            height: 8,
            depth: 1,
            row_pitch: 256,
        };
        let to = Location(readback.as_raw().cast_const().cast(), 1, rows);
        let texture = depth_res.as_raw().cast_const().cast();
        let from = Location(
            texture,
            0,
            PlacedFootprint {
                offset: 0,
                ..NO_FOOTPRINT
            },
        );
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            let (to, from) = ((&raw const to).cast(), (&raw const from).cast());
            second.list.CopyTextureRegion(to, 0, 0, 0, from, None)
        };
        second.close();
        host.execute(&[&second]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); 8 rows of 256 bytes.
        let read = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            std::slice::from_raw_parts(mapped.cast::<u16>(), 8 * 128)
        };
        for y in 0..8 {
            for x in 0..8 {
                let value: f32 = match x {
                    0..2 => 0.5,
                    2..4 => 0.75,
                    _ => 0.25,
                };
                let wanted = (value * 65535.0).round() as i32;
                let got = i32::from(read[y * 128 + x]);
                assert!(
                    (got - wanted).abs() <= 1,
                    "texel ({x}, {y}): {got:#x}, wanted {value} ({wanted:#x})"
                );
            }
        }
        drop(depth_res);
        assert_eq!(slot::<Release>(depth, 0x10)(depth), 0);
    }
}
