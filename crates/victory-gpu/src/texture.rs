//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::memstat::{self, Kind};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
struct Staging {
    buffer: ID3D12Resource,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    last: Mutex<Last>,
    /// The staging buffer's size, as counted in [`memstat`].
    bytes: u64,
}

impl Drop for Staging {
    fn drop(&mut self) {
        memstat::free(Kind::TextureStaging, self.bytes);
    }
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
}

/// Buffers texture memory is read into before it is detiled, shared by all
/// textures (each used to keep one of its own size for good: a second
/// system-memory copy of every title-written texture). Kept initialized,
/// so reuse costs no clearing: every read fills what it uses.
static READS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// At most this many pooled read buffers ...
const READ_BUFFERS: usize = 4;
/// ... of at most this many bytes in all; past it the largest go first.
const READ_BYTES: usize = 128 << 20;

/// A read buffer of at least `size` bytes; `None` if none can be made.
fn take_read(size: usize) -> Option<Vec<u8>> {
    let mut pool = READS.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.len() >= size)
        .min_by_key(|(_, v)| v.len())
        .map(|(i, _)| i);
    if let Some(i) = fit {
        return Some(pool.swap_remove(i));
    }
    drop(pool);
    let mut v = Vec::new();
    v.try_reserve_exact(size).ok()?;
    v.resize(size, 0);
    memstat::alloc(Kind::TextureRead, size as u64);
    Some(v)
}

/// Gives a read buffer back to the pool, which keeps within its bounds.
fn give_read(v: Vec<u8>) {
    let mut pool = READS.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(Vec::len);
    let mut total: usize = pool.iter().map(Vec::len).sum();
    while pool.len() > READ_BUFFERS || total > READ_BYTES {
        let Some(largest) = pool.pop() else { break };
        total -= largest.len();
        memstat::free(Kind::TextureRead, largest.len() as u64);
    }
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock (by that thread, or by workers it waits for, each on its own
// subresources); the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Bytes of a texture past which its subresources detile on several
/// threads. shortcut: not measured; tune from an authorized run's texture
/// preparing times (`VICTORY_COPY_WORKERS=1` for the serial baseline).
const PARALLEL_DETILE: usize = 8 << 20;

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    for (i, footprint) in staging.footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    list.defer_host(move || staging.refresh());
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some((now, written)) = crate::watch::writes(start, start + size, false) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            // Read through the copy, which stands unmapped pages (zeros):
            // memory of big pools is too slow to ask page by page.
            let Some(mut read) = take_read(size) else {
                once!(size, "no memory to read a texture into; not detiled");
                return 0;
            };
            last.asked = now;
            crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
            self.detile(&read[..size]);
            give_read(read);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return 0;
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        // Subresources detile into disjoint parts of the staging buffer, so
        // a big texture's are shared among a few threads.
        let subresources: Vec<usize> = (0..self.footprints.len()).collect();
        crate::par::for_each(
            &subresources,
            crate::par::workers(),
            PARALLEL_DETILE,
            |&i| self.subresource_len(i),
            |&i| {
                let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
                let footprint = &self.footprints[i];
                let pitch = footprint.Footprint.RowPitch as usize;
                // SAFETY: subresource `i`'s own part of the mapped staging
                // buffer; no other item writes it, nothing reads it now.
                let linear = unsafe {
                    std::slice::from_raw_parts_mut(
                        self.data.add(footprint.Offset as usize),
                        self.subresource_len(i),
                    )
                };
                let depth_pitch = pitch * self.rows[i] as usize;
                if let Err(e) = self
                    .layout
                    .detile(tiled, mip, slice, linear, pitch, depth_pitch)
                {
                    tracing::warn!(?e, mip, slice, "texture subresource not detiled");
                }
            },
        );
    }

    /// Bytes of subresource `i` in the staging buffer.
    fn subresource_len(&self, i: usize) -> usize {
        let f = self.footprints[i].Footprint;
        f.RowPitch as usize * self.rows[i] as usize * f.Depth as usize
    }
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        buffer,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        memory,
        last: Mutex::default(),
        bytes: total,
    });
    memstat::alloc(Kind::TextureStaging, total);
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};

    /// Every subresource of a texture big enough to detile on several
    /// threads lands, row for row, where the host texture's footprints say.
    #[test]
    fn detiling_fills_every_subresource() {
        let Some(host) = host() else {
            return;
        };
        let (width, mips, slices, bpe) = (512u32, 3u32, 2u32, 16u32);
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width,
            height: width,
            depth: slices,
            mips,
            element_bytes: bpe,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_64KB_D,
        })
        .expect("layout");
        assert!(
            layout.size as usize > PARALLEL_DETILE,
            "detiles in parallel"
        );
        let d = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: width,
            DepthOrArraySize: slices as u16,
            MipLevels: mips as u16,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32B32A32_UINT,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let count = (mips * slices) as usize;
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count];
        let mut rows = vec![0u32; count];
        let mut total = 0;
        // SAFETY: outputs for `count` subresources.
        unsafe {
            host.device.GetCopyableFootprints(
                &d,
                0,
                count as u32,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            );
        }
        let (buffer, data) = host.upload_buffer(total).expect("staging");
        memstat::alloc(Kind::TextureStaging, total);
        let staging = Staging {
            buffer,
            data,
            footprints,
            rows,
            layout: layout.clone(),
            mips,
            memory: std::ptr::null(),
            last: Mutex::default(),
            bytes: total,
        };

        // Distinct contents per subresource and texel, tiled as the console.
        let mut tiled = vec![0u8; layout.size as usize];
        let texel = |i: usize, x: usize, y: usize| (i * 131 + x * 7 + y * 13) as u8;
        let linear_of = |i: usize| {
            let w = (width >> (i as u32 % mips)) as usize;
            let mut v = vec![0u8; w * w * bpe as usize];
            for y in 0..w {
                for x in 0..w * bpe as usize {
                    v[y * w * bpe as usize + x] = texel(i, x, y);
                }
            }
            (w, v)
        };
        for i in 0..count {
            let (w, linear) = linear_of(i);
            let pitch = w * bpe as usize;
            let (mip, slice) = (i as u32 % mips, i as u32 / mips);
            layout
                .tile(&mut tiled, mip, slice, &linear, pitch, pitch * w)
                .expect("tile");
        }
        staging.detile(&tiled);
        for i in 0..count {
            let (w, linear) = linear_of(i);
            let f = staging.footprints[i];
            let pitch = f.Footprint.RowPitch as usize;
            for y in 0..w {
                // SAFETY: row `y` of subresource `i` in the mapped staging.
                let row = unsafe {
                    std::slice::from_raw_parts(
                        staging.data.add(f.Offset as usize + y * pitch),
                        w * bpe as usize,
                    )
                };
                assert_eq!(row, &linear[y * w * bpe as usize..][..w * bpe as usize]);
            }
        }
    }

    /// Read buffers are reused when big enough and the pool keeps within
    /// its bounds; a buffer bigger than the whole budget is not kept.
    #[test]
    fn read_buffers_are_pooled_within_bounds() {
        let small = take_read(4096).expect("read buffer");
        assert!(small.len() >= 4096);
        give_read(small);
        memstat::alloc(Kind::TextureRead, READ_BYTES as u64 + 1);
        give_read(vec![0; READ_BYTES + 1]);
        let pool = READS.lock().unwrap();
        assert!(pool.len() <= READ_BUFFERS);
        assert!(pool.iter().map(Vec::len).sum::<usize>() <= READ_BYTES);
        assert!(pool.iter().any(|v| v.len() >= 4096), "the small one kept");
    }
}
