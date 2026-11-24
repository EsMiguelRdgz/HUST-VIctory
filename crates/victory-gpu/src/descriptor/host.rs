//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;
use crate::shadow::Shadow;

pub(crate) struct HostHeap {
    device: ID3D12Device,
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic_count` more.
    count: u32,
    /// Title's originally requested descriptor count (may exceed `count` when
    /// capped to the host's shader-visible heap limit).
    requested: u32,
    /// Number of dynamic slots appended after `count` (0 for sampler heaps).
    dynamic_count: u32,
    /// Whether `count < requested` because the host capped the shader-visible
    /// view heap.
    capped: bool,
    sampler: bool,
    /// Owning title `Heap` pointer (for registering deferred texture slots).
    title_heap: usize,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// Host copies of title memory that slots' views are made on and that
    /// nothing else keeps (loose windows and aligned copies, see
    /// [`crate::shadow::Shadow::kept_by_views`]): held while a slot shows
    /// them, so reclaiming unused copies never frees one a slot reads.
    owners: Mutex<HashMap<u32, Arc<Shadow>>>,
    /// Host textures that slots' SRV/UAV descriptors point to: `(title_resource,
    /// unordered, host_texture, view)`. Keeps textures alive while bound in a
    /// slot and lets eviction/release re-null with a typed NULL and release
    /// slots cleanly.
    tex_owners: Mutex<HashMap<u32, (usize, bool, ID3D12Resource, View)>>,
    /// Active-list guard cloned into any open `HostList` that binds a bindless
    /// (`ResourceDescriptorHeap[]`) pipeline against this heap. While
    /// `Arc::strong_count > 1`, eviction never mutates or frees textures bound
    /// in this heap.
    bindless_guard: Arc<()>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// Diagnostic counters for capped heap activity and fallbacks.
    capped_mirrors: AtomicU64,
    dynamic_exhausted: AtomicU64,
    null_fallbacks: AtomicU64,
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2, and on drivers (such as NVIDIA) that enforce 1,000,000
/// shader-visible CBV/SRV/UAV descriptors even on tier 3.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw, and descriptor tables whose
/// indices exceed a capped host heap). A list holds the slots it took
/// until it is reset (its submissions have run) or released.
const DYNAMIC: u32 = 1 << 16;

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32, title_heap: usize) -> Option<Self> {
        let host = host()?;
        Self::new_on_device(
            host.device.clone(),
            sampler,
            count,
            PORTABLE_VIEWS,
            DYNAMIC,
            title_heap,
        )
    }

    /// Creates a shader-visible host heap on `device`, capping view heaps at
    /// `max_views` if `device` rejects `requested + dynamic_count` (or if
    /// `force_cap` is true in tests).
    fn new_on_device(
        device: ID3D12Device,
        sampler: bool,
        requested: u32,
        max_views: u32,
        dynamic_slots: u32,
        title_heap: usize,
    ) -> Option<Self> {
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |num: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: num,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
            }
        };
        // A view heap reserves `dynamic_slots` dynamic slots plus 1 permanent
        // null guard slot at the end (`guard_slot = count + dynamic_count`)
        // so shaders with OOB computed heap indices read null instead of
        // wrapping onto an unrelated descriptor on NVIDIA GPUs (PR #483).
        let extra = if sampler { 0 } else { dynamic_slots };
        let total_extra = if sampler { 0 } else { dynamic_slots + 1 };
        let desired = requested.saturating_add(total_extra);
        let (heap, count, capped) = match make(desired) {
            Ok(heap) => (heap, requested, false),
            Err(e) if !sampler && desired > max_views && max_views > total_extra => {
                let capped_count = max_views - total_extra;
                tracing::warn!(
                    error = %e,
                    requested,
                    host_limit = max_views,
                    mirrored = capped_count,
                    dynamic = extra,
                    "host view heap capped; tables past mirrored limit will remap into dynamic slots"
                );
                (make(max_views).ok()?, capped_count, true)
            }
            Err(e) => {
                tracing::warn!(error = %e, requested, sampler, "no host descriptor heap");
                return None;
            }
        };
        Self::finish_init(
            device, heap, kind, sampler, requested, count, extra, capped, title_heap,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_capped_for_test(
        device: ID3D12Device,
        requested: u32,
        host_limit: u32,
        dynamic_slots: u32,
    ) -> Option<Self> {
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        assert!(host_limit > dynamic_slots);
        // Allocate 1 extra slot past `host_limit` for the permanent null guard
        // slot while keeping mirrored+dynamic counts identical to `host_limit`.
        // SAFETY: plain heap creation on the test device.
        let heap = unsafe {
            device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: host_limit + 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?
        };
        let count = (host_limit - dynamic_slots).min(requested);
        let capped = count < requested;
        Self::finish_init(
            device,
            heap,
            kind,
            false,
            requested,
            count,
            dynamic_slots,
            capped,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_init(
        device: ID3D12Device,
        heap: ID3D12DescriptorHeap,
        kind: D3D12_DESCRIPTOR_HEAP_TYPE,
        sampler: bool,
        requested: u32,
        count: u32,
        dynamic_count: u32,
        capped: bool,
        title_heap: usize,
    ) -> Option<Self> {
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        // SAFETY: a live heap.
        let slots = unsafe { heap.GetDesc() }.NumDescriptors;
        crate::host::track(
            &heap,
            crate::memstat::Kind::DescriptorHeap,
            u64::from(slots) * u64::from(step),
        );
        tracing::debug!(
            count,
            requested,
            dynamic_count,
            capped,
            sampler,
            "host descriptor heap"
        );
        let h = HostHeap {
            device,
            heap,
            cpu,
            step,
            count,
            requested,
            dynamic_count,
            capped,
            sampler,
            title_heap,
            made: Mutex::new(vec![0; count as usize]),
            owners: Mutex::default(),
            tex_owners: Mutex::default(),
            bindless_guard: Arc::new(()),
            next: Mutex::new((0, vec![false; dynamic_count as usize])),
            capped_mirrors: AtomicU64::new(0),
            dynamic_exhausted: AtomicU64::new(0),
            null_fallbacks: AtomicU64::new(0),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot (including
        // the trailing guard slot) starts null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, slots) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    /// Number of directly mirrored slots (`0..count`). View indices `>= count`
    /// must be remapped into dynamic slots (`count..count + dynamic_count`).
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The slot translated shaders read in place of a computed heap index at
    /// or past it (an OOB index wraps on NVIDIA GPUs and can hang the device):
    /// in a view heap its trailing slot after the dynamic range, null from the
    /// start and never made; in a sampler heap its last slot.
    pub(crate) fn guard_slot(&self) -> u32 {
        if self.sampler {
            self.count.saturating_sub(1)
        } else {
            self.count + self.dynamic_count
        }
    }

    #[allow(dead_code)]
    pub(crate) fn requested(&self) -> u32 {
        self.requested
    }

    #[allow(dead_code)]
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    /// Number of dynamic slots currently held by in-flight command lists.
    pub(crate) fn dynamic_in_use(&self) -> u32 {
        let guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        guard.1.iter().filter(|&&used| used).count() as u32
    }

    /// Concise summary of heap bounds and counters for diagnostics / DRED.
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> String {
        format!(
            "kind={} mirrored={} requested={} dynamic={}/{} capped={} capped_mirrors={} dynamic_exhausted={} null_fallbacks={}",
            if self.sampler { "sampler" } else { "view" },
            self.count,
            self.requested,
            self.dynamic_in_use(),
            self.dynamic_count,
            self.capped,
            self.capped_mirrors.load(Ordering::Relaxed),
            self.dynamic_exhausted.load(Ordering::Relaxed),
            self.null_fallbacks.load(Ordering::Relaxed),
        )
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        if total == 0 {
            return Ok(());
        }
        let block_size = total.min(1 << 16);
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                self.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: block_size,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler_on(&self.device, start);
            } else {
                null_view_on(&self.device, start);
            }
            let mut filled = 1;
            while filled < block_size {
                let n = filled.min(block_size - filled);
                self.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = block_size.min(total - done);
                self.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler_on(&self.device, self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler_on(device: &ID3D12Device, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic_count {
            return None;
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let dyn_len = self.dynamic_count as usize;
        let n = n as usize;
        // Search from `next` on, then from the start.
        let starts = (*next as usize..dyn_len).chain(0..*next as usize);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            self.dynamic_exhausted.fetch_add(1, Ordering::Relaxed);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    n,
                    dynamic = self.dynamic_count,
                    "no free dynamic descriptor slots: draw cannot bind remapped table"
                )
            });
            return None;
        };
        taken[first..first + n].fill(true);
        *next = ((first + n) % dyn_len) as u32;
        Some(self.count + first as u32)
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned), re-nulling them before making them available again so stale
    /// descriptors never linger across frames.
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let end = at.saturating_add(n).min(self.dynamic_count);
        // Re-null before clearing `taken` so no concurrent allocator observes
        // a partially cleared slot.
        for slot in at..end {
            self.make_null(self.count + slot);
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slots) = guard.1.get_mut(at as usize..end as usize) {
            slots.fill(false);
        }
    }

    /// Active-list guard cloned into any open command list using direct
    /// `ResourceDescriptorHeap[]` indexing on this heap.
    pub(crate) fn bindless_guard(&self) -> Arc<()> {
        self.bindless_guard.clone()
    }

    /// Whether an open or executing bindless command list currently uses this
    /// heap and this heap holds a live descriptor pointing to `resource`.
    pub(crate) fn pins_texture_for_bindless(&self, resource: usize) -> bool {
        if self.sampler || Arc::strong_count(&self.bindless_guard) == 1 {
            return false;
        }
        let tex = self.tex_owners.lock().unwrap_or_else(|e| e.into_inner());
        tex.values().any(|&(res, ..)| res == resource)
    }

    /// Makes slot `index` (a dynamic one) from `view`; a type-compatible null
    /// view if the host cannot. Returns `(title_resource, host_texture, list_guard)`
    /// if a host texture was bound into the slot so the caller can pin it on the
    /// open command list with zero gap against concurrent placed-texture eviction.
    pub(crate) fn make_at(
        &self,
        index: u32,
        view: &View,
    ) -> Option<(usize, ID3D12Resource, Arc<()>)> {
        let mut owner = None;
        let mut tex_owner = None;
        let mut tex_guard = None;
        if !make_owned(
            &self.device,
            view,
            self.cpu(index),
            &mut owner,
            &mut tex_owner,
            &mut tex_guard,
        ) {
            self.null_fallbacks.fetch_add(1, Ordering::Relaxed);
            null_for_on(&self.device, view, self.cpu(index));
        }
        let bound = tex_owner.as_ref().and_then(|(res, _, tex, _)| {
            let guard = tex_guard
                .take()
                .or_else(|| crate::resource::list_guard(*res as crate::object::This))?;
            Some((*res, tex.clone(), guard))
        });
        self.own(index, owner, tex_owner);
        bound
    }

    /// Slot `index` now shows `owner`'s buffer copy and/or `tex_owner`'s host
    /// texture (or none).
    fn own(
        &self,
        index: u32,
        owner: Option<Arc<Shadow>>,
        tex_owner: Option<(usize, bool, ID3D12Resource, View)>,
    ) {
        let mut owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        match owner {
            Some(shadow) => owners.insert(index, shadow),
            None => owners.remove(&index),
        };
        drop(owners);
        let mut tex = self.tex_owners.lock().unwrap_or_else(|e| e.into_inner());
        match tex_owner {
            Some(entry) => tex.insert(index, entry),
            None => tex.remove(&index),
        };
    }

    /// Counts mirrored (`slot < self.count`) buffer-shadow references held by
    /// this heap's `owners` map into `counts`, keyed by `Arc::as_ptr(s) as usize`.
    /// Returns `false` if an open bindless command list currently pins this heap
    /// (in which case callers must not evict buffer descriptors from it).
    pub(crate) fn count_mirrored_buffers(
        &self,
        counts: &mut std::collections::HashMap<usize, usize>,
    ) -> bool {
        if self.sampler {
            return true;
        }
        if Arc::strong_count(&self.bindless_guard) > 1 {
            return false;
        }
        let owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        for (&slot, shadow) in owners.iter() {
            if slot < self.count {
                *counts.entry(Arc::as_ptr(shadow) as usize).or_insert(0) += 1;
            }
        }
        true
    }

    /// Evicts mirrored (`slot < self.count`) buffer descriptors whose shadow
    /// pointer is in `targets`: resets `made[slot] = 0`, writes a NULL view,
    /// and drops this heap's `Arc<Shadow>` reference (outside heap locks).
    /// Never touches dynamic slots (`slot >= self.count`) or heaps pinned by an
    /// active bindless list. Returns the number of evicted descriptor slots.
    pub(crate) fn evict_buffers(&self, targets: &std::collections::HashSet<usize>) -> usize {
        if self.sampler || targets.is_empty() || Arc::strong_count(&self.bindless_guard) > 1 {
            return 0;
        }
        let mut dropped: Vec<Arc<Shadow>> = Vec::new();
        {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let _depth = MadeDepthGuard::enter();
            let mut owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
            let slots: Vec<u32> = owners
                .iter()
                .filter_map(|(&slot, shadow)| {
                    (slot < self.count && targets.contains(&(Arc::as_ptr(shadow) as usize)))
                        .then_some(slot)
                })
                .collect();
            for slot in slots {
                if let Some(shadow) = owners.remove(&slot) {
                    dropped.push(shadow);
                }
                made[slot as usize] = 0;
                null_view_on(&self.device, self.cpu(slot));
            }
        }
        let count = dropped.len();
        drop(dropped);
        count
    }

    /// Re-nulls every slot pointing to `resource` (optionally restricted to
    /// slots holding the exact evicted host `ID3D12Resource` pointer
    /// `only_host_ptr`), writing a type- and dimension-compatible NULL
    /// descriptor, dropping this heap's COM reference, resetting mirrored
    /// `made` keys so future draws rebuild the slot, and returning the
    /// mirrored `(slot, unordered)` pairs that were evicted.
    pub(crate) fn evict_texture(
        &self,
        resource: usize,
        only_host_ptr: Option<usize>,
    ) -> Vec<(u32, bool)> {
        use windows::core::Interface;
        if self.sampler {
            return Vec::new();
        }
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let mut tex = self.tex_owners.lock().unwrap_or_else(|e| e.into_inner());
        let slots: Vec<(u32, bool)> = tex
            .iter()
            .filter_map(|(&slot, (res, unordered, host_tex, _))| {
                if *res != resource {
                    return None;
                }
                if let Some(target_ptr) = only_host_ptr
                    && host_tex.as_raw() as usize != target_ptr
                {
                    return None;
                }
                Some((slot, *unordered))
            })
            .collect();
        let mut mirrored = Vec::new();
        for (slot, unordered) in slots {
            let removed = tex.remove(&slot);
            if slot < self.count {
                made[slot as usize] = 0;
                mirrored.push((slot, unordered));
            }
            match removed {
                Some((_, _, _, view)) => null_for_on(&self.device, &view, self.cpu(slot)),
                None => null_view_on(&self.device, self.cpu(slot)),
            }
        }
        mirrored
    }

    /// Host copies the slots keep alive (diagnostics and tests).
    #[cfg(test)]
    fn owned(&self, index: u32) -> Option<Arc<Shadow>> {
        let owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        owners.get(&index).cloned()
    }

    /// Makes slot `index` a type-compatible null view for `view`.
    pub(crate) fn make_null_for(&self, view: &View, index: u32) {
        self.null_fallbacks.fetch_add(1, Ordering::Relaxed);
        null_for_on(&self.device, view, self.cpu(index));
        self.own(index, None, None);
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        self.own(index, None, None);
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view_on(&self.device, self.cpu(index));
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory, clamping `size` to D3D12's valid 256-byte-aligned `[256, 65536]`
    /// range.
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.own(index, None, None);
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: cbv_size(size),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            self.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    pub(crate) fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            self.capped_mirrors.fetch_add(1, Ordering::Relaxed);
            if for_draw {
                once!(
                    index,
                    count = self.count,
                    requested = self.requested,
                    "descriptor mirror requested past capped host heap limit"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        if let Some(ref v) = view {
            if for_draw {
                // Materialize any deferred texture or placed texture BEFORE
                // taking `self.made` so `on_texture_materialized` /
                // `PLACED_TEXTURES` never re-enter or invert `self.made`.
                ensure_view_materialized(v);
            } else if !crate::resource::lazy_textures_enabled()
                && matches!(v, View::Placed(false, r, _) if r[0] != 1)
            {
                // Eager rollback mode (`VICTORY_LAZY_TEXTURES=0`): resolve
                // placed textures before locking `self.made` to keep lock
                // order strictly acyclic (`PLACED_TEXTURES` never under `made`).
                let _ = placed_texture(v);
            }
        }
        let deferred_to_register = {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let _depth = MadeDepthGuard::enter();
            let Some(ref view) = view else {
                // Not a view of ours (never written, or of a resource the
                // title released): nothing to read.
                if made[index as usize] != 0 {
                    made[index as usize] = 0;
                    self.make_null(index);
                }
                return None;
            };
            let mut register = None;
            if made[index as usize] != key {
                let deferred_tex = (!for_draw).then(|| deferred_texture_of(view)).flatten();
                let deferred_placed = !for_draw
                    && crate::resource::lazy_textures_enabled()
                    && matches!(view, View::Placed(false, r, _) if r[0] != 1);
                let later =
                    (!for_draw && view.is_buffer()) || deferred_tex.is_some() || deferred_placed;
                made[index as usize] = if later { 0 } else { key };
                let mut owner = None;
                let mut tex_owner = None;
                let mut tex_guard = None;
                if (later
                    || !make_owned(
                        &self.device,
                        view,
                        self.cpu(index),
                        &mut owner,
                        &mut tex_owner,
                        &mut tex_guard,
                    ))
                    && !self.sampler
                {
                    if !later {
                        self.null_fallbacks.fetch_add(1, Ordering::Relaxed);
                    }
                    null_for_on(&self.device, view, self.cpu(index));
                }
                self.own(index, owner, tex_owner);
                register = deferred_tex;
            }
            register
        };
        if let Some((res, unordered)) = deferred_to_register {
            super::register_deferred_slot(res, self.title_heap, index, handle, unordered);
        }
        view
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        if index >= self.count {
            if !self.sampler && index < self.count + self.dynamic_count {
                return Some(format!(
                    "dynamic slot +{} (in-flight remapped table)",
                    index - self.count
                ));
            }
            if !self.sampler && index == self.count + self.dynamic_count {
                return Some("guard slot (null)".to_string());
            }
            return Some(format!(
                "OOB past host heap (count={}, requested={})",
                self.count, self.requested
            ));
        }
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Returns `(resource, is_uav)` if `view` names a title texture whose host
/// D3D12 texture has not been materialized yet.
fn deferred_texture_of(view: &View) -> Option<(usize, bool)> {
    match *view {
        View::Shader(r, _)
            if r != 0 && crate::resource::is_deferred_texture(r as crate::object::This) =>
        {
            Some((r, false))
        }
        View::Unordered(r, _, _)
            if r != 0 && crate::resource::is_deferred_texture(r as crate::object::This) =>
        {
            Some((r, true))
        }
        _ => None,
    }
}

thread_local! {
    /// Nesting depth of `HostHeap.made` held by the current thread so
    /// `shadow::relieve_pressure` never re-enters `HostHeap::evict_texture` on
    /// the same thread if an inner allocation triggers memory-pressure relief.
    static MADE_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Whether the current thread holds a `HostHeap.made` lock.
pub(crate) fn in_heap_made_lock() -> bool {
    MADE_LOCK_DEPTH.with(|d| d.get() > 0)
}

struct MadeDepthGuard;

impl MadeDepthGuard {
    fn enter() -> Self {
        MADE_LOCK_DEPTH.with(|d| d.set(d.get() + 1));
        Self
    }
}

impl Drop for MadeDepthGuard {
    fn drop(&mut self) {
        MADE_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Ensures any deferred texture, placed texture, or buffer shadow referenced by
/// `view` is materialized BEFORE taking `HostHeap.made`, keeping lock order
/// strictly acyclic (`shadow::relieve_pressure` -> `evict_idle_textures` ->
/// `HostHeap::evict_texture` -> `HostHeap.made` never re-enters `HostHeap.made`).
fn ensure_view_materialized(view: &View) {
    if let Some((r, _)) = deferred_texture_of(view) {
        let _ = crate::resource::host_resource(r as crate::object::This);
    } else if matches!(view, View::Placed(false, r, _) if r[0] != 1) {
        let _ = placed_texture(view);
    } else if let Some((address, len, align)) = buffer_access(view) {
        let _ = crate::shadow::find_aligned(address, len.max(1), align);
    }
}

/// Valid D3D12 CBV `SizeInBytes`: 256-byte aligned and in `[256, 65536]`.
pub(crate) fn cbv_size(size: u32) -> u32 {
    size.clamp(1, 65_536).next_multiple_of(256)
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
#[cfg(feature = "diagnostics")]
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (addr, len, _) = buffer_access(view)?;
    Some((addr, len))
}

/// The title memory range and element alignment `(address, len, element_size)`
/// of a buffer SRV/UAV view.
pub(crate) fn buffer_access(view: &View) -> Option<(u64, u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_access(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_access(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    if element == 0 {
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
        element,
    ))
}

/// [`buffer_access`] of an unordered access view of memory at `base`.
fn unordered_access(base: u64, words: &[u32; 10]) -> Option<(u64, u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    if element == 0 {
        return None;
    }
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
        element,
    ))
}

/// `shadow` if a descriptor made on it must keep it alive.
fn keep(shadow: Arc<Shadow>, owner: &mut Option<Arc<Shadow>>) {
    if shadow.kept_by_views() {
        *owner = Some(shadow);
    }
}

fn buffer_srv(
    device: &ID3D12Device,
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    owner: &mut Option<Arc<Shadow>>,
) -> bool {
    let element = element_size(&desc);
    if element == 0 {
        once!(element, "buffer view with zero element size");
        return false;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let view_end = buffer
        .FirstElement
        .saturating_add(u64::from(buffer.NumElements))
        .saturating_mul(element);
    let needed = len.max(view_end).max(element);
    let Some(shadow) = crate::shadow::find_aligned(address, needed, element) else {
        return false;
    };
    let offset = shadow.offset(address);
    if !offset.is_multiple_of(element) {
        once!(
            offset,
            element,
            "buffer view not on an element of its host copy"
        );
        return false;
    }
    buffer.FirstElement += offset / element;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe { device.CreateShaderResourceView(&shadow.buffer, Some(&desc), at) };
    keep(shadow, owner);
    true
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    device: &ID3D12Device,
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    owner: &mut Option<Arc<Shadow>>,
) -> bool {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    if element == 0 {
        once!(element, "unordered buffer view with zero element size");
        return false;
    }
    let view_end = buffer
        .FirstElement
        .saturating_add(u64::from(buffer.NumElements))
        .saturating_mul(element);
    let needed = len.max(view_end).max(element);
    let Some(shadow) = crate::shadow::find_aligned(address, needed, element) else {
        return false;
    };
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return false;
    }
    let offset = shadow.offset(address);
    if !offset.is_multiple_of(element) {
        once!(
            offset,
            element,
            "unordered buffer view not on an element of its host copy"
        );
        return false;
    }
    buffer.FirstElement += offset / element;
    buffer.CounterOffsetInBytes = 0;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        device.CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    keep(shadow, owner);
    true
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    let promoted = (own == 87 && crate::resource::is_packed_16bit_color(view))
        || (own == 10 && crate::resource::is_shared_exp_color(view));
    if family(view) != family(own) && !promoted {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && (!typeless || promoted)
    {
        *format = d.Format;
    }
    true
}

/// Makes the host descriptor for `view` at `at` on `device`; false if it cannot.
#[cfg_attr(not(test), allow(dead_code))]
fn make_on(device: &ID3D12Device, view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> bool {
    make_owned(device, view, at, &mut None, &mut None, &mut None)
}

/// As [`make_on`], setting `owner` to the host copy the descriptor reads if
/// the descriptor must keep it alive, `tex_owner` to any host texture bound
/// into the slot, and `tex_guard` to any placed-texture active-use guard
/// keeping the underlying `Resource` alive until the caller pins it.
fn make_owned(
    device: &ID3D12Device,
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    owner: &mut Option<Arc<Shadow>>,
    tex_owner: &mut Option<(usize, bool, ID3D12Resource, View)>,
    tex_guard: &mut Option<Arc<()>>,
) -> bool {
    match view {
        &View::Constant(address, size) => {
            let Some(shadow) = crate::shadow::find(address, u64::from(size.max(1))) else {
                return false;
            };
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return false;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: cbv_size(size),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            keep(shadow, owner);
            true
        }
        View::Shader(0, Some(words)) => {
            // Standard D3D12 typed NULL SRV (pResource == NULL, pDesc != NULL).
            let desc = sanitize_null_srv(srv_desc(words));
            // SAFETY: a slot of our heap; null resource is valid with a sanitized desc.
            unsafe { device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at) };
            true
        }
        View::Shader(resource, desc) => {
            let res_id = *resource;
            let resource = res_id as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(&texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return false;
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        &texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                *tex_owner = Some((res_id, false, texture, view.clone()));
                true
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(device, data as u64, len, srv_desc(desc), at, owner)
            } else {
                false
            }
        }
        View::Unordered(0, _, Some(words)) => {
            // Standard D3D12 typed NULL UAV (pResource == NULL, pDesc != NULL),
            // preserving the title's exact ViewDimension (e.g. Texture1D) and format.
            let desc = sanitize_null_uav(uav_desc(words));
            // SAFETY: a slot of our heap; null resource is valid with a sanitized desc.
            unsafe {
                device.CreateUnorderedAccessView(
                    None::<&ID3D12Resource>,
                    None::<&ID3D12Resource>,
                    Some(&desc),
                    at,
                )
            };
            true
        }
        View::Unordered(resource, _counter, desc) => {
            let res_id = *resource;
            let resource = res_id as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(device, data as u64, len, uav_desc(desc), at, owner)
                    }
                    _ => false,
                };
            };
            let mut desc = desc.as_ref().map(uav_desc);
            if !fits(&texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return false;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    &texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            *tex_owner = Some((res_id, true, texture, view.clone()));
            true
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            true
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            true
        }
        View::Placed(unordered, resource, desc) => {
            if let Some((view, guard)) = placed_texture(view) {
                let ok = make_owned(device, &view, at, owner, tex_owner, tex_guard);
                if ok {
                    *tex_guard = Some(guard);
                }
                return ok;
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(
                    dimension = resource[0],
                    "placed unordered texture view not mirrored"
                );
                return false;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if *unordered {
                buffer_uav(device, address, len, uav_desc(words), at, owner)
            } else {
                buffer_srv(device, address, len, srv_desc(words), at, owner)
            }
        }
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use), paired with an
/// active-use guard that prevents concurrent eviction from freeing the
/// `Resource` while the caller creates descriptors or pins it onto a list.
pub(crate) fn placed_texture(view: &View) -> Option<(View, Arc<()>)> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let (texture, guard) = crate::resource::texture_at(address, resource)?.into_parts();
    Some((
        View::Shader(texture as usize, desc[..10].try_into().ok()),
        guard,
    ))
}

/// Sanitizes an SRV description so `CreateShaderResourceView(None, Some(&desc), at)`
/// is guaranteed valid under D3D12 across all drivers (never passing console-only
/// formats, typeless/BC-on-1D/MS mismatches, or invalid component-mapping bits)
/// while preserving the shader's expected descriptor dimension and buffer kind
/// (raw / structured / typed).
fn sanitize_null_srv(desc: D3D12_SHADER_RESOURCE_VIEW_DESC) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    match desc.ViewDimension {
        D3D12_SRV_DIMENSION_BUFFER => {
            // SAFETY: ViewDimension is BUFFER.
            let b = unsafe { desc.Anonymous.Buffer };
            let (format, flags, stride) = if b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
                (DXGI_FORMAT_R32_TYPELESS, D3D12_BUFFER_SRV_FLAG_RAW, 0)
            } else if b.StructureByteStride > 0 && b.StructureByteStride <= 2048 {
                (
                    DXGI_FORMAT_UNKNOWN,
                    D3D12_BUFFER_SRV_FLAG_NONE,
                    b.StructureByteStride,
                )
            } else {
                (DXGI_FORMAT_R32_UINT, D3D12_BUFFER_SRV_FLAG_NONE, 0)
            };
            D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: mapping,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: 1,
                        StructureByteStride: stride,
                        Flags: flags,
                    },
                },
            }
        }
        D3D12_SRV_DIMENSION_TEXTURE1D => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE1D,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture1D: D3D12_TEX1D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE1DARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE1DARRAY,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture1DArray: D3D12_TEX1D_ARRAY_SRV {
                    MipLevels: 1,
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2D => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                    MipLevels: 1,
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2DMS => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DMS,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DMS: D3D12_TEX2DMS_SRV::default(),
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DMSArray: D3D12_TEX2DMS_ARRAY_SRV {
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE3D => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE3D,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture3D: D3D12_TEX3D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURECUBE => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURECUBE,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                TextureCube: D3D12_TEXCUBE_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURECUBEARRAY,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                TextureCubeArray: D3D12_TEXCUBE_ARRAY_SRV {
                    MipLevels: 1,
                    NumCubes: 1,
                    ..Default::default()
                },
            },
        },
        _ => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        },
    }
}

/// Sanitizes a UAV description so `CreateUnorderedAccessView(None, None, Some(&desc), at)`
/// is guaranteed valid under D3D12 across all drivers while preserving the
/// shader's expected descriptor dimension and buffer kind.
fn sanitize_null_uav(desc: D3D12_UNORDERED_ACCESS_VIEW_DESC) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    match desc.ViewDimension {
        D3D12_UAV_DIMENSION_BUFFER => {
            // SAFETY: ViewDimension is BUFFER.
            let b = unsafe { desc.Anonymous.Buffer };
            let (format, flags, stride) = if b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
                (DXGI_FORMAT_R32_TYPELESS, D3D12_BUFFER_UAV_FLAG_RAW, 0)
            } else if b.StructureByteStride > 0 && b.StructureByteStride <= 2048 {
                (
                    DXGI_FORMAT_UNKNOWN,
                    D3D12_BUFFER_UAV_FLAG_NONE,
                    b.StructureByteStride,
                )
            } else {
                (DXGI_FORMAT_R32_UINT, D3D12_BUFFER_UAV_FLAG_NONE, 0)
            };
            D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: 0,
                        NumElements: 1,
                        StructureByteStride: stride,
                        CounterOffsetInBytes: 0,
                        Flags: flags,
                    },
                },
            }
        }
        D3D12_UAV_DIMENSION_TEXTURE1D => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE1D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture1D: D3D12_TEX1D_UAV::default(),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE1DARRAY => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE1DARRAY,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture1DArray: D3D12_TEX1D_ARRAY_UAV {
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE2D => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_UAV::default(),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE2DARRAY => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE3D => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE3D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture3D: D3D12_TEX3D_UAV {
                    WSize: 1,
                    ..Default::default()
                },
            },
        },
        _ => D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        },
    }
}

/// A type- and dimension-compatible null descriptor matching `view`, for a
/// slot whose view the host cannot make. Reads return zeros and writes are
/// dropped without causing a SM 6.6 `ResourceDescriptorHeap` type mismatch.
fn null_for_on(device: &ID3D12Device, view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    match view {
        View::Constant(..) => {
            // SAFETY: null CBV description is valid in D3D12.
            unsafe { device.CreateConstantBufferView(None, at) };
        }
        View::Shader(_, Some(words)) => {
            let desc = sanitize_null_srv(srv_desc(words));
            // SAFETY: null SRV with sanitized desc is valid in D3D12.
            unsafe { device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at) };
        }
        View::Shader(_, None) => null_view_on(device, at),
        View::Placed(false, resource, desc) => {
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            let parsed = srv_desc(words);
            let sanitized = if parsed.ViewDimension != D3D12_SRV_DIMENSION_UNKNOWN {
                sanitize_null_srv(parsed)
            } else if resource[0] == 1 {
                D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                }
            } else {
                sanitize_null_srv(parsed)
            };
            // SAFETY: null SRV with sanitized desc is valid in D3D12.
            unsafe {
                device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&sanitized), at)
            };
        }
        View::Unordered(r, _, desc) => {
            let sanitized = match desc {
                Some(words) => sanitize_null_uav(uav_desc(words)),
                None => {
                    let buffer = crate::resource::buffer_bytes(*r as crate::object::This).is_some();
                    D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_UINT,
                        ViewDimension: if buffer {
                            D3D12_UAV_DIMENSION_BUFFER
                        } else {
                            D3D12_UAV_DIMENSION_TEXTURE2D
                        },
                        ..Default::default()
                    }
                }
            };
            // SAFETY: null UAV with sanitized desc is valid in D3D12.
            unsafe {
                device.CreateUnorderedAccessView(
                    None::<&ID3D12Resource>,
                    None::<&ID3D12Resource>,
                    Some(&sanitized),
                    at,
                )
            };
        }
        View::Placed(true, resource, desc) => {
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            let parsed = uav_desc(words);
            let sanitized = if parsed.ViewDimension != D3D12_UAV_DIMENSION_UNKNOWN {
                sanitize_null_uav(parsed)
            } else {
                D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_UINT,
                    ViewDimension: if resource[0] == 1 {
                        D3D12_UAV_DIMENSION_BUFFER
                    } else {
                        D3D12_UAV_DIMENSION_TEXTURE2D
                    },
                    ..Default::default()
                }
            };
            // SAFETY: null UAV with sanitized desc is valid in D3D12.
            unsafe {
                device.CreateUnorderedAccessView(
                    None::<&ID3D12Resource>,
                    None::<&ID3D12Resource>,
                    Some(&sanitized),
                    at,
                )
            };
        }
        View::Sampler(_) | View::ConsoleSampler(_) => null_sampler_on(device, at),
    }
}

/// A view that reads zeros, for slots whose view the host cannot make.
fn null_view_on(device: &ID3D12Device, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe { device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4, 0) else {
            return; // no host GPU
        };
        let a = heap.dynamic(DYNAMIC - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + DYNAMIC - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + DYNAMIC - 40));
    }

    #[test]
    fn cbv_size_clamps_and_aligns_to_d3d12_limits() {
        assert_eq!(cbv_size(0), 256);
        assert_eq!(cbv_size(1), 256);
        assert_eq!(cbv_size(256), 256);
        assert_eq!(cbv_size(257), 512);
        assert_eq!(cbv_size(65_536), 65_536);
        assert_eq!(cbv_size(128_000), 65_536);
    }

    #[test]
    fn capped_heap_tracks_bounds_and_dynamic_exhaustion_on_warp() {
        let Some(device) = crate::host::warp_device() else {
            return;
        };
        let heap =
            HostHeap::new_capped_for_test(device, 128, 64, 16).expect("capped WARP host heap");
        assert!(heap.capped());
        assert_eq!(heap.requested(), 128);
        assert_eq!(heap.count(), 48);
        assert_eq!(heap.dynamic_in_use(), 0);

        // Mirroring past the capped mirrored range must fail cleanly and record a counter.
        assert!(heap.mirror(50, 0, true).is_none());
        assert!(heap.stats().contains("capped=true"));
        assert!(heap.stats().contains("capped_mirrors=1"));

        // Dynamic slots live in [48..64) and are re-nulled on release.
        let a = heap.dynamic(10).expect("10 dynamic slots");
        assert_eq!(a, 48);
        assert_eq!(heap.dynamic_in_use(), 10);
        let b = heap.dynamic(6).expect("remaining 6 dynamic slots");
        assert_eq!(b, 58);
        assert_eq!(heap.dynamic_in_use(), 16);

        // Exhaustion returns None and increments counter instead of wrapping.
        assert_eq!(heap.dynamic(1), None);
        assert!(heap.stats().contains("dynamic_exhausted=1"));

        heap.release(a, 10);
        assert_eq!(heap.dynamic_in_use(), 6);
        let c = heap.dynamic(10).expect("reclaimed 10 dynamic slots");
        assert_eq!(c, 48);
        heap.release(c, 10);
        heap.release(b, 6);
        assert_eq!(heap.dynamic_in_use(), 0);
    }

    #[test]
    fn typed_null_descriptors_and_fallbacks_preserve_dimensions_on_warp() {
        let Some(device) = crate::host::warp_device() else {
            return;
        };
        let heap =
            HostHeap::new_capped_for_test(device.clone(), 32, 32, 8).expect("WARP host heap");

        // Exact typed NULL UAV observed in crash logs: R32_UINT (42), TEXTURE1D (2).
        let tex1d_uav_words = [42, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        let sanitized_uav = sanitize_null_uav(uav_desc(&tex1d_uav_words));
        assert_eq!(sanitized_uav.Format, DXGI_FORMAT_R32_UINT);
        assert_eq!(sanitized_uav.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE1D);
        assert!(make_on(
            &device,
            &View::Unordered(0, 0, Some(tex1d_uav_words)),
            heap.cpu(0),
        ));

        // Raw buffer NULL UAV & Structured buffer NULL UAV.
        // Layout: [0]=Format, [1]=ViewDimension, [2..4]=FirstElement(u64),
        // [4]=NumElements, [5]=StructureByteStride, [6..8]=CounterOffsetInBytes(u64),
        // [8]=Flags.
        let raw_uav_words = [
            DXGI_FORMAT_R32_TYPELESS.0 as u32,
            D3D12_UAV_DIMENSION_BUFFER.0 as u32,
            0,
            0,
            64,
            0,
            128, // non-zero CounterOffsetInBytes must be cleared for NULL UAV
            0,
            D3D12_BUFFER_UAV_FLAG_RAW.0 as u32,
            0,
        ];
        let sanitized_raw = sanitize_null_uav(uav_desc(&raw_uav_words));
        assert_eq!(sanitized_raw.Format, DXGI_FORMAT_R32_TYPELESS);
        assert_eq!(sanitized_raw.ViewDimension, D3D12_UAV_DIMENSION_BUFFER);
        // SAFETY: ViewDimension is BUFFER.
        let counter_offset = unsafe { sanitized_raw.Anonymous.Buffer.CounterOffsetInBytes };
        assert_eq!(counter_offset, 0);
        assert!(make_on(
            &device,
            &View::Unordered(0, 0, Some(raw_uav_words)),
            heap.cpu(1),
        ));

        let structured_uav_words = [
            DXGI_FORMAT_UNKNOWN.0 as u32,
            D3D12_UAV_DIMENSION_BUFFER.0 as u32,
            0,
            0,
            16,
            32, // StructureByteStride
            0,
            0,
            0,
            0,
        ];
        assert!(make_on(
            &device,
            &View::Unordered(0, 0, Some(structured_uav_words)),
            heap.cpu(2),
        ));

        // Typed NULL SRVs: raw buffer, structured buffer, depth format, typeless, cube, 3D.
        // Layout: [0]=Format, [1]=ViewDimension, [2]=Mapping, [3]=pad,
        // [4..6]=FirstElement(u64), [6]=NumElements, [7]=StructureByteStride, [8]=Flags.
        let raw_srv_words = [
            DXGI_FORMAT_R32_TYPELESS.0 as u32,
            D3D12_SRV_DIMENSION_BUFFER.0 as u32,
            D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            0,
            0,
            0,
            16,
            0,
            D3D12_BUFFER_SRV_FLAG_RAW.0 as u32,
            0,
        ];
        let sanitized_raw_srv = sanitize_null_srv(srv_desc(&raw_srv_words));
        assert_eq!(sanitized_raw_srv.Format, DXGI_FORMAT_R32_TYPELESS);
        // SAFETY: ViewDimension is BUFFER.
        let srv_flags = unsafe { sanitized_raw_srv.Anonymous.Buffer.Flags };
        assert_eq!(srv_flags, D3D12_BUFFER_SRV_FLAG_RAW);
        assert!(make_on(
            &device,
            &View::Shader(0, Some(raw_srv_words)),
            heap.cpu(3),
        ));

        let depth_srv_words = [
            DXGI_FORMAT_D32_FLOAT.0 as u32,
            D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32,
            0, // missing force-set bit should be sanitized
            0,
            1,
            0,
            0,
            0,
            0,
            0,
        ];
        let sanitized_depth = sanitize_null_srv(srv_desc(&depth_srv_words));
        assert_eq!(sanitized_depth.Format, DXGI_FORMAT_R8G8B8A8_UNORM);
        assert_eq!(
            sanitized_depth.Shader4ComponentMapping,
            D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
        );
        assert!(make_on(
            &device,
            &View::Shader(0, Some(depth_srv_words)),
            heap.cpu(4),
        ));

        let cube_srv_words = [
            DXGI_FORMAT_R8G8B8A8_TYPELESS.0 as u32,
            D3D12_SRV_DIMENSION_TEXTURECUBE.0 as u32,
            D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
        ];
        let sanitized_cube = sanitize_null_srv(srv_desc(&cube_srv_words));
        assert_eq!(sanitized_cube.Format, DXGI_FORMAT_R8G8B8A8_UNORM);
        assert_eq!(
            sanitized_cube.ViewDimension,
            D3D12_SRV_DIMENSION_TEXTURECUBE
        );
        assert!(make_on(
            &device,
            &View::Shader(0, Some(cube_srv_words)),
            heap.cpu(5),
        ));

        // Console-only DXGI formats, BC format on 1D/2DMS, garbage component
        // mapping bits, and oversized buffer stride must all sanitize cleanly
        // without triggering D3D12 Debug Layer errors or device removal.
        for (slot, (fmt, dim, mapping)) in [
            (116u32, D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32, 0xFFFF_FFFF),
            (
                117,
                D3D12_SRV_DIMENSION_TEXTURE2DARRAY.0 as u32,
                0xFFFF_FFFF,
            ),
            (189, D3D12_SRV_DIMENSION_TEXTURE3D.0 as u32, 0x1000),
            (190, D3D12_SRV_DIMENSION_TEXTURE1D.0 as u32, 0xFFFF_1000),
            (191, D3D12_SRV_DIMENSION_TEXTURE2DMS.0 as u32, 0),
            (
                DXGI_FORMAT_BC7_UNORM.0 as u32,
                D3D12_SRV_DIMENSION_TEXTURE1D.0 as u32,
                0xFFFF_FFFF,
            ),
            (
                DXGI_FORMAT_BC7_UNORM.0 as u32,
                D3D12_SRV_DIMENSION_TEXTURE2DMS.0 as u32,
                0xFFFF_FFFF,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let words = [fmt, dim, mapping, 0, 1, 0, 0, 0, 0, 0];
            let s = sanitize_null_srv(srv_desc(&words));
            assert_eq!(s.Format, DXGI_FORMAT_R8G8B8A8_UNORM);
            assert_eq!(
                s.Shader4ComponentMapping,
                D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
            );
            assert!(make_on(
                &device,
                &View::Shader(0, Some(words)),
                heap.cpu(9 + slot as u32),
            ));
        }

        // Oversized buffer stride must fall back to typed R32_UINT NULL buffer.
        let bad_stride_srv = [
            DXGI_FORMAT_UNKNOWN.0 as u32,
            D3D12_SRV_DIMENSION_BUFFER.0 as u32,
            0xFFFF_FFFF,
            0,
            0,
            0,
            0,
            0xFFFF_FFFF, // oversized StructureByteStride
            0,
            0,
        ];
        let sanitized_bad_srv = sanitize_null_srv(srv_desc(&bad_stride_srv));
        assert_eq!(sanitized_bad_srv.Format, DXGI_FORMAT_R32_UINT);
        assert!(make_on(
            &device,
            &View::Shader(0, Some(bad_stride_srv)),
            heap.cpu(16),
        ));

        let bad_stride_uav = [
            DXGI_FORMAT_UNKNOWN.0 as u32,
            D3D12_UAV_DIMENSION_BUFFER.0 as u32,
            0,
            0,
            0,
            0xFFFF_FFFF, // oversized StructureByteStride
            999,
            0,
            0,
            0,
        ];
        let sanitized_bad_uav = sanitize_null_uav(uav_desc(&bad_stride_uav));
        assert_eq!(sanitized_bad_uav.Format, DXGI_FORMAT_R32_UINT);
        assert!(make_on(
            &device,
            &View::Unordered(0, 0, Some(bad_stride_uav)),
            heap.cpu(17),
        ));

        // Null fallbacks for CBV, placed buffer SRV, and placed texture UAV.
        null_for_on(&device, &View::Constant(0x1000, 0), heap.cpu(6));
        let mut placed_res = [0u32; 14];
        placed_res[0] = 1; // buffer
        let mut placed_desc = [0u32; 16];
        placed_desc[..10].copy_from_slice(&raw_srv_words);
        null_for_on(
            &device,
            &View::Placed(false, placed_res, placed_desc),
            heap.cpu(7),
        );
        placed_res[0] = 3; // texture2d
        placed_desc[..10].copy_from_slice(&tex1d_uav_words);
        null_for_on(
            &device,
            &View::Placed(true, placed_res, placed_desc),
            heap.cpu(8),
        );

        // SAFETY: plain device health check.
        assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok());
        crate::host::assert_no_debug_errors(&device);
    }

    #[test]
    fn misaligned_structured_buffer_srv_and_uav_succeed_via_aligned_shadow() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let heap =
            HostHeap::new_capped_for_test(host_gpu.device.clone(), 16, 16, 4).expect("host heap");

        // Backing title buffer of 64 KiB; create structured buffer views at
        // offset 16 with element stride 32 (16 % 32 != 0, matching console.txt).
        let backing = vec![0x5Au8; 64 * 1024];
        let base = backing.as_ptr() as u64;
        let fake_res_id = (base as usize) ^ 0xDEAD_BEEF;
        crate::shadow::register(fake_res_id, base, backing.len() as u64);

        let view_addr = base + 16;
        let mut res_desc = [0u32; 14];
        res_desc[0] = 1; // BUFFER
        res_desc[4] = 4096; // width low

        // Placed structured SRV: FirstElement=2, NumElements=32, StructureByteStride=32.
        let mut srv_desc_words = [0u32; 16];
        srv_desc_words[0] = DXGI_FORMAT_UNKNOWN.0 as u32;
        srv_desc_words[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        srv_desc_words[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        srv_desc_words[4] = 2; // FirstElement low
        srv_desc_words[6] = 32; // NumElements
        srv_desc_words[7] = 32; // StructureByteStride
        srv_desc_words[10] = view_addr as u32;
        srv_desc_words[11] = (view_addr >> 32) as u32;
        let srv_view = View::Placed(false, res_desc, srv_desc_words);

        // Placed structured UAV: FirstElement=2, NumElements=32, StructureByteStride=32.
        let mut uav_desc_words = [0u32; 16];
        uav_desc_words[0] = DXGI_FORMAT_UNKNOWN.0 as u32;
        uav_desc_words[1] = D3D12_UAV_DIMENSION_BUFFER.0 as u32;
        uav_desc_words[2] = 2; // FirstElement low
        uav_desc_words[4] = 32; // NumElements
        uav_desc_words[5] = 32; // StructureByteStride
        uav_desc_words[10] = view_addr as u32;
        uav_desc_words[11] = (view_addr >> 32) as u32;
        let uav_view = View::Placed(true, res_desc, uav_desc_words);

        assert!(
            make_on(&host_gpu.device, &srv_view, heap.cpu(0)),
            "misaligned structured buffer SRV must succeed via aligned shadow"
        );
        assert!(
            make_on(&host_gpu.device, &uav_view, heap.cpu(1)),
            "misaligned structured buffer UAV must succeed via aligned shadow"
        );

        let (srv_start, srv_len, srv_align) = buffer_access(&srv_view).expect("buffer access");
        assert_eq!(
            (srv_start, srv_len, srv_align),
            (view_addr + 64, 32 * 32, 32)
        );
        let base_shadow = crate::shadow::find_aligned(view_addr, 4096, 32).expect("base aligned");
        let range_shadow =
            crate::shadow::find_aligned(srv_start, srv_len, srv_align).expect("range aligned");
        assert_eq!(base_shadow.offset(view_addr) % 32, 0);
        assert!(
            std::sync::Arc::ptr_eq(&base_shadow, &range_shadow),
            "descriptor creation and draw::readable must resolve to the same aligned shadow"
        );
        crate::host::assert_no_debug_errors(&host_gpu.device);

        crate::shadow::unregister(fake_res_id, base);
    }

    /// A slot made on an aligned copy keeps that copy alive until the slot
    /// shows something else; slots on buffer-object copies keep nothing.
    #[test]
    fn slots_keep_the_copies_they_need_alive() {
        let Some(host_gpu) = crate::host::host() else {
            return;
        };
        let heap =
            HostHeap::new_capped_for_test(host_gpu.device.clone(), 16, 16, 4).expect("host heap");
        let backing = vec![0u8; 64 * 1024];
        let base = backing.as_ptr() as u64;
        let fake_res_id = (base as usize) ^ 0x0BE7_0A11;
        crate::shadow::register(fake_res_id, base, backing.len() as u64);

        let view_addr = base + 16;
        let mut res_desc = [0u32; 14];
        res_desc[0] = 1; // BUFFER
        res_desc[4] = 4096;
        let mut words = [0u32; 16];
        words[0] = DXGI_FORMAT_UNKNOWN.0 as u32;
        words[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        words[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        words[6] = 32; // NumElements
        words[7] = 32; // StructureByteStride
        words[10] = view_addr as u32;
        words[11] = (view_addr >> 32) as u32;
        heap.make_at(0, &View::Placed(false, res_desc, words));
        let owner = heap.owned(0).expect("aligned copy kept");
        assert!(owner.kept_by_views());

        // Buffer-object copies are also kept by slots so pressure reclamation
        // can safely free idle buffer copies without breaking live descriptors.
        words[10] = base as u32;
        words[11] = (base >> 32) as u32;
        heap.make_at(1, &View::Placed(false, res_desc, words));
        let buf_owner = heap.owned(1).expect("buffer copy kept while bound");
        assert!(buf_owner.kept_by_views());

        let weak = Arc::downgrade(&owner);
        let buf_weak = Arc::downgrade(&buf_owner);
        drop(owner);
        drop(buf_owner);
        crate::shadow::unregister(fake_res_id, base); // prunes buffer and aligned maps
        assert!(weak.upgrade().is_some(), "slot 0 still reads aligned copy");
        assert!(
            buf_weak.upgrade().is_some(),
            "slot 1 still reads buffer copy"
        );
        heap.make_null(0);
        assert!(heap.owned(0).is_none());
        assert!(weak.upgrade().is_none(), "aligned copy freed once nulled");
        heap.make_null(1);
        assert!(heap.owned(1).is_none());
        assert!(
            buf_weak.upgrade().is_none(),
            "buffer copy freed once nulled"
        );
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }

    /// Regression test for the Test #11/#14 `HostHeap.made` self-deadlock:
    /// calling `shadow::relieve_pressure` while the current thread holds
    /// `HostHeap.made` must skip texture eviction cleanly instead of
    /// re-entering `HostHeap::evict_texture` on `self.made`.
    #[test]
    fn relieve_pressure_inside_heap_made_lock_never_deadlocks() {
        let Some(host_gpu) = host() else {
            return;
        };
        let heap = HostHeap::new(false, 8, 0).expect("host view heap");
        let _made = heap.made.lock().unwrap();
        let _depth = MadeDepthGuard::enter();
        assert!(in_heap_made_lock());
        // Must return without re-entering `heap.made` via `evict_texture`.
        let _ = crate::shadow::relieve_pressure(0);
        drop(_depth);
        drop(_made);
        assert!(!in_heap_made_lock());
        crate::host::assert_no_debug_errors(&host_gpu.device);
    }
}
