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

/// A texture's upload state: the host texture's subresource layout in an
/// upload buffer, and what was last uploaded. The upload buffer itself is
/// borrowed from [`UPLOADS`] only for a submission that finds the contents
/// changed (it used to be kept for good: a full-size system-memory copy of
/// every title-written texture, besides the host texture).
pub(crate) struct Staging {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    last: Mutex<Last>,
    /// Bytes of upload memory the subresources take.
    total: u64,
}

/// What the host texture was last given: the hash of the memory detiled
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

/// Upload buffers lent to submissions that upload texture contents, kept
/// for reuse: at most [`UPLOAD_BUFFERS`] of at most [`UPLOAD_BYTES`] in all
/// (largest freed first), none at all under memory pressure.
static UPLOADS: Mutex<Vec<Lent>> = Mutex::new(Vec::new());
const UPLOAD_BUFFERS: usize = 16;
const UPLOAD_BYTES: u64 = 256 << 20;

/// An upload buffer (mapped) and its size.
struct Lent {
    buffer: ID3D12Resource,
    data: *mut u8,
    size: u64,
}

// SAFETY: D3D12 resources are free-threaded; the mapping is written only by
// the submission the buffer is lent to.
unsafe impl Send for Lent {}

/// An upload buffer of at least `size` bytes: the smallest pooled one that
/// fits, else a new one (counted as texture staging while it lives).
fn lend(size: u64) -> Option<Lent> {
    let mut pool = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, l)| l.size >= size)
        .min_by_key(|(_, l)| l.size)
        .map(|(i, _)| i);
    if let Some(i) = fit {
        memstat::count(memstat::Event::PoolHits, 1);
        return Some(pool.swap_remove(i));
    }
    drop(pool);
    memstat::count(memstat::Event::PoolMisses, 1);
    if memstat::under_pressure(size) {
        crate::shadow::relieve_pressure(size);
    }
    let Some((buffer, data)) = host()?.upload_buffer(size) else {
        memstat::count(memstat::Event::Failures, 1);
        return None;
    };
    crate::host::track(&buffer, Kind::TextureStaging, size);
    Some(Lent { buffer, data, size })
}

/// Takes an upload buffer back once the GPU is done with it.
fn give_back(lent: Lent) {
    let mut pool = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(lent);
    pool.sort_by_key(|l| l.size);
    let mut total: u64 = pool.iter().map(|l| l.size).sum();
    let keep = if memstat::under_pressure(0) {
        0
    } else {
        UPLOAD_BYTES
    };
    while pool.len() > UPLOAD_BUFFERS || total > keep {
        let Some(largest) = pool.pop() else { break };
        total -= largest.size;
    }
}

/// Frees every pooled upload buffer (memory pressure); returns the bytes.
pub(crate) fn drain_pools() -> u64 {
    let lent: u64 = std::mem::take(&mut *UPLOADS.lock().unwrap_or_else(|e| e.into_inner()))
        .iter()
        .map(|l| l.size)
        .sum();
    let reads: Vec<Vec<u8>> = std::mem::take(&mut *READS.lock().unwrap_or_else(|e| e.into_inner()));
    let read: u64 = reads.iter().map(|v| v.len() as u64).sum();
    for v in &reads {
        memstat::free(Kind::TextureRead, v.len() as u64);
    }
    lent + read
}

/// New contents for a host texture, detiled at submission into a lent
/// upload buffer: copied into the texture by the submission's prelude, the
/// buffer given back when this is dropped (after the submission ran).
pub(crate) struct Upload {
    staging: Arc<Staging>,
    texture: ID3D12Resource,
    pub(crate) resource: usize,
    lent: Option<Lent>,
}

impl Upload {
    /// Records the copies of every subresource into the texture, which must
    /// be in the copy destination state.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList) {
        let Some(lent) = &self.lent else { return };
        for (i, footprint) in self.staging.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(&self.texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: i as u32,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&lent.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: *footprint,
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }

    /// The host texture.
    pub(crate) fn texture(&self) -> &ID3D12Resource {
        &self.texture
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(lent) = self.lent.take() {
            give_back(lent);
        }
    }
}

/// Upload buffers recorded into submissions, for tests.
#[cfg(test)]
pub(crate) static RECORDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// SAFETY: the title memory is only read; the lent upload buffer is written
// only at submission, under `last`'s lock (by that thread, or by workers it
// waits for, each on its own subresources).
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

/// Whether the host GPU has written `resource` (via copy or resolve), so its
/// authoritative contents live on the host rather than in title memory.
pub(crate) fn is_host_written(resource: This) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)))
}

/// Clears the last-uploaded state for `resource` so that if an evicted
/// title-written host texture is later re-materialized, its next use re-uploads
/// from title memory.
pub(crate) fn reset_uploaded(resource: usize) {
    let staging = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref().and_then(|m| m.get(&resource).cloned())
    };
    if let Some(s) = staging {
        let mut last = s.last.lock().unwrap_or_else(|e| e.into_inner());
        last.hash = None;
        last.asked = 0;
    }
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

/// Brings `resource`'s host copy up to date when `list` is submitted, once
/// per list: if its memory changed since the last upload, the submission's
/// prelude copies the new contents in before the list runs.
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
    list.defer_texture(
        staging,
        resource as usize,
        texture.clone(),
        crate::resource::list_guard(resource),
    );
}

impl Staging {
    /// At submission: new contents for the host texture if the memory
    /// changed since the last upload (and is mapped at all), detiled into a
    /// lent upload buffer.
    pub(crate) fn refresh(
        self: &Arc<Self>,
        resource: usize,
        texture: &ID3D12Resource,
    ) -> Option<Upload> {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let (read, lent) = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
        #[cfg(test)]
        if lent.is_some() {
            RECORDED.fetch_add(1, Relaxed);
        }
        lent.map(|lent| Upload {
            staging: self.clone(),
            texture: texture.clone(),
            resource,
            lent: Some(lent),
        })
    }

    /// Returns the bytes of title memory read, and the detiled contents if
    /// they changed. What was uploaded is noted only once the contents are
    /// in a buffer, so a failed allocation retries next time.
    fn refresh_now(&self) -> (usize, Option<Lent>) {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some((now, written)) = crate::watch::writes(start, start + size, false) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return (0, None);
            }
            // Read through the copy, which stands unmapped pages (zeros):
            // memory of big pools is too slow to ask page by page.
            let Some(mut read) = take_read(size) else {
                once!(size, "no memory to read a texture into; not detiled");
                memstat::count(memstat::Event::Failures, 1);
                return (0, None);
            };
            crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
            let lent = self.detile_lent(&read[..size]);
            give_read(read);
            if lent.is_some() {
                last.asked = now;
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return (size, lent);
        }
        if !committed(self.memory, size) {
            if !partially_committed_mapped(self.memory, size) {
                #[cfg(feature = "diagnostics")]
                tracing::debug!(
                    memory = ?self.memory,
                    size,
                    desc = ?self.layout.desc,
                    "a sampled texture's memory is not committed"
                );
                return (0, None);
            }
            // Demand-committed physical-page texture with some committed pages
            // and untouched trailing zero pages: read committed pages and fill
            // uncommitted pages with zeros without forcing commit on them.
            let Some(mut read) = take_read(size) else {
                once!(size, "no memory to read a texture into; not detiled");
                memstat::count(memstat::Event::Failures, 1);
                return (0, None);
            };
            crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
            let hash = blake3::hash(&read[..size]);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            if last.hash == Some(hash) {
                give_read(read);
                return (size, None);
            }
            let lent = self.detile_lent(&read[..size]);
            give_read(read);
            if lent.is_some() {
                last.hash = Some(hash);
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            return (size, lent);
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return (size, None);
        }
        let lent = self.detile_lent(tiled);
        if lent.is_some() {
            last.hash = Some(hash);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        (size, lent)
    }

    /// Detiles `tiled` into a lent upload buffer; `None` if none is had.
    fn detile_lent(&self, tiled: &[u8]) -> Option<Lent> {
        let Some(lent) = lend(self.total) else {
            once!(
                total = self.total,
                "no upload memory for a texture's contents; retried at the next submission"
            );
            return None;
        };
        self.detile(tiled, lent.data);
        Some(lent)
    }

    /// Detiles `tiled` (the texture's memory) into the upload buffer mapped
    /// at `data` ([`Staging::total`] bytes), laid out as the footprints say.
    fn detile(&self, tiled: &[u8], data: *mut u8) {
        let data = data as usize;
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
                // SAFETY: subresource `i`'s own part of the mapped upload
                // buffer; no other item writes it, nothing reads it now.
                let linear = unsafe {
                    std::slice::from_raw_parts_mut(
                        (data as *mut u8).add(footprint.Offset as usize),
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

/// Whether `[at, at + len)` is entirely `MEM_MAPPED` (mapped physical-page
/// views) with at least one `MEM_COMMIT` page. Returns `false` if any page is
/// unmapped (`MEM_FREE`) or a placeholder (`MEM_PRIVATE`), or if none of the
/// pages have been written/committed yet.
fn partially_committed_mapped(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let mut p = at as usize;
    let end = p.saturating_add(len);
    let mut any_committed = false;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.RegionSize == 0 || info.Type != MEM_MAPPED {
            return false;
        }
        if info.State == MEM_COMMIT {
            any_committed = true;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= p {
            break;
        }
        p = next;
    }
    any_committed
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
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        memory,
        last: Mutex::default(),
        total,
    });
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
        let staging = Staging {
            footprints,
            rows,
            layout: layout.clone(),
            mips,
            memory: std::ptr::null(),
            last: Mutex::default(),
            total,
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
        staging.detile(&tiled, data);
        for i in 0..count {
            let (w, linear) = linear_of(i);
            let f = staging.footprints[i];
            let pitch = f.Footprint.RowPitch as usize;
            for y in 0..w {
                // SAFETY: row `y` of subresource `i` in the mapped staging.
                let row = unsafe {
                    std::slice::from_raw_parts(
                        data.add(f.Offset as usize + y * pitch),
                        w * bpe as usize,
                    )
                };
                assert_eq!(row, &linear[y * w * bpe as usize..][..w * bpe as usize]);
            }
        }
        drop(buffer);
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
