//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap;
//! overlap them with per-submission fences once more than clears run.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

/// Host D3D12 adapter and feature capabilities, queried once when the host
/// device is created. Compatibility decisions branch on these detected
/// capabilities rather than assuming console and PC GPUs are identical.
#[derive(Debug, Clone)]
pub(crate) struct HostCaps {
    pub(crate) adapter_name: String,
    pub(crate) vendor_id: u32,
    pub(crate) device_id: u32,
    pub(crate) dedicated_vram: u64,
    pub(crate) warp: bool,
    pub(crate) highest_shader_model: D3D_SHADER_MODEL,
    pub(crate) resource_binding_tier: D3D12_RESOURCE_BINDING_TIER,
    pub(crate) resource_heap_tier: D3D12_RESOURCE_HEAP_TIER,
    pub(crate) typed_uav_load_additional_formats: bool,
    pub(crate) wave_ops: bool,
    pub(crate) wave_lane_count_min: u32,
    pub(crate) wave_lane_count_max: u32,
    pub(crate) mesh_shader_tier: D3D12_MESH_SHADER_TIER,
    pub(crate) enhanced_barriers: bool,
    pub(crate) max_gpu_va_bits_per_resource: u32,
    pub(crate) max_gpu_va_bits_per_process: u32,
    pub(crate) uma: bool,
    pub(crate) existing_heaps: bool,
}

impl HostCaps {
    pub(crate) fn query(device: &ID3D12Device, warp_requested: bool) -> Self {
        // SAFETY: each CheckFeatureSupport call passes a properly sized struct
        // matching its D3D12_FEATURE discriminator.
        unsafe {
            let (adapter_name, vendor_id, device_id, dedicated_vram, software) =
                adapter_info(device).unwrap_or_else(|| ("unknown".to_owned(), 0, 0, 0, false));

            let mut highest_shader_model = D3D_SHADER_MODEL_5_1;
            for candidate in [
                D3D_SHADER_MODEL_6_7,
                D3D_SHADER_MODEL_6_6,
                D3D_SHADER_MODEL_6_5,
                D3D_SHADER_MODEL_6_0,
            ] {
                let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                    HighestShaderModel: candidate,
                };
                if device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_SHADER_MODEL,
                        (&raw mut model).cast(),
                        size_of_val(&model) as u32,
                    )
                    .is_ok()
                {
                    highest_shader_model = model.HighestShaderModel;
                    break;
                }
            }

            let mut opts = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            let _ = device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut opts).cast(),
                size_of_val(&opts) as u32,
            );

            let mut opts1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            let _ = device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut opts1).cast(),
                size_of_val(&opts1) as u32,
            );

            let mut opts7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let _ = device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut opts7).cast(),
                size_of_val(&opts7) as u32,
            );

            let mut opts12 = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
            let enhanced_barriers = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS12,
                    (&raw mut opts12).cast(),
                    size_of_val(&opts12) as u32,
                )
                .is_ok()
                && opts12.EnhancedBarriersSupported.as_bool();

            let mut va = D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT::default();
            let _ = device.CheckFeatureSupport(
                D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT,
                (&raw mut va).cast(),
                size_of_val(&va) as u32,
            );

            let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE1::default();
            let _ = device.CheckFeatureSupport(
                D3D12_FEATURE_ARCHITECTURE1,
                (&raw mut arch).cast(),
                size_of_val(&arch) as u32,
            );

            let mut existing = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
            let existing_heaps = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_EXISTING_HEAPS,
                    (&raw mut existing).cast(),
                    size_of_val(&existing) as u32,
                )
                .is_ok()
                && existing.Supported.as_bool();

            HostCaps {
                adapter_name,
                vendor_id,
                device_id,
                dedicated_vram,
                warp: warp_requested || software,
                highest_shader_model,
                resource_binding_tier: opts.ResourceBindingTier,
                resource_heap_tier: opts.ResourceHeapTier,
                typed_uav_load_additional_formats: opts.TypedUAVLoadAdditionalFormats.as_bool(),
                wave_ops: opts1.WaveOps.as_bool(),
                wave_lane_count_min: opts1.WaveLaneCountMin,
                wave_lane_count_max: opts1.WaveLaneCountMax,
                mesh_shader_tier: opts7.MeshShaderTier,
                enhanced_barriers,
                max_gpu_va_bits_per_resource: va.MaxGPUVirtualAddressBitsPerResource,
                max_gpu_va_bits_per_process: va.MaxGPUVirtualAddressBitsPerProcess,
                uma: arch.UMA.as_bool(),
                existing_heaps,
            }
        }
    }

    pub(crate) fn supports_shader_model_6_6(&self) -> bool {
        self.highest_shader_model.0 >= D3D_SHADER_MODEL_6_6.0
    }

    pub(crate) fn supports_mesh_shaders(&self) -> bool {
        self.supports_shader_model_6_6()
            && self.mesh_shader_tier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
    }

    pub(crate) fn supports_wave_size(&self, required: Option<u32>) -> bool {
        match required {
            None => true,
            Some(n) => {
                self.wave_ops && self.wave_lane_count_min <= n && n <= self.wave_lane_count_max
            }
        }
    }

    pub(crate) fn summary(&self) -> String {
        let sm_major = (self.highest_shader_model.0 >> 4) & 0xf;
        let sm_minor = self.highest_shader_model.0 & 0xf;
        format!(
            "adapter=\"{}\" ({:04x}:{:04x}, vram={}MiB, warp={}) sm={sm_major}.{sm_minor} bind_tier={} heap_tier={} wave={}..{}(ops={}) mesh_tier={} typed_uav={} enhanced_barriers={} va={}/{}b uma={} existing_heaps={}",
            self.adapter_name,
            self.vendor_id,
            self.device_id,
            self.dedicated_vram >> 20,
            self.warp,
            self.resource_binding_tier.0,
            self.resource_heap_tier.0,
            self.wave_lane_count_min,
            self.wave_lane_count_max,
            self.wave_ops,
            self.mesh_shader_tier.0,
            self.typed_uav_load_additional_formats,
            self.enhanced_barriers,
            self.max_gpu_va_bits_per_resource,
            self.max_gpu_va_bits_per_process,
            self.uma,
            self.existing_heaps,
        )
    }
}

unsafe fn adapter_info(device: &ID3D12Device) -> Option<(String, u32, u32, u64, bool)> {
    // SAFETY: GetAdapterLuid and DXGI adapter lookup by LUID are plain queries.
    unsafe {
        let luid = device.GetAdapterLuid();
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        let software = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        Some((
            name,
            desc.VendorId,
            desc.DeviceId,
            desc.DedicatedVideoMemory as u64,
            software,
        ))
    }
}

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) caps: HostCaps,
    format_support: Mutex<HashMap<i32, (D3D12_FORMAT_SUPPORT1, D3D12_FORMAT_SUPPORT2)>>,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// The commit guard's end of the run: a host failure report.
fn stop_for_memory(text: &str) -> ! {
    fail(format_args!("{text}"))
}

/// The host GPU's memory use and budget, for the memory monitor.
fn video_memory() -> Option<crate::memstat::Video> {
    host().and_then(Host::video_memory)
}

/// Counts `bytes` of `kind` for as long as the D3D12 object lives.
pub(crate) fn track(object: &impl Interface, kind: crate::memstat::Kind, bytes: u64) {
    if let Ok(object) = object.cast::<ID3D12Object>() {
        crate::memstat::track(&object, kind, bytes);
    }
}

/// How long a failed fence-event wait falls back to polling the fence.
const UNCONFIRMED_POLL: std::time::Duration = std::time::Duration::from_secs(10);

/// Submissions whose completion could not be confirmed.
static UNCONFIRMED_WAITS: AtomicU64 = AtomicU64::new(0);

/// Waits until `fence` reaches `value` and returns whether that was
/// confirmed. `signalled`: the queue accepted the signal (if not, the value
/// is never reached and only an already completed fence counts). The wait
/// goes through `event` for up to `timeout_ms`; if arming or waiting fails,
/// or the (auto-reset, shared) event woke early, the fence is polled for up
/// to `poll`. A completed value of `u64::MAX` is a removed device, not
/// completion.
fn wait_fence(
    fence: &ID3D12Fence,
    event: HANDLE,
    value: u64,
    signalled: bool,
    timeout_ms: u32,
    poll: std::time::Duration,
) -> bool {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    // SAFETY: a plain query of a live fence.
    let done = || match unsafe { fence.GetCompletedValue() } {
        u64::MAX => Some(false),
        v if v >= value => Some(true),
        _ => None,
    };
    if let Some(done) = done() {
        return done;
    }
    if !signalled {
        return false;
    }
    // SAFETY: our fence and event.
    match unsafe { fence.SetEventOnCompletion(value, event) } {
        Ok(()) => {
            // SAFETY: our event handle.
            let waited = unsafe { WaitForSingleObject(event, timeout_ms) };
            if waited == WAIT_OBJECT_0
                && let Some(done) = done()
            {
                return done;
            }
            tracing::warn!(?waited, value, "host fence wait failed; polling the fence");
        }
        Err(e) => {
            tracing::warn!(error = %e, value, "host fence event not armed; polling the fence")
        }
    }
    let deadline = std::time::Instant::now() + poll;
    loop {
        if let Some(done) = done() {
            return done;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, changed bytes), then of that the microseconds
/// writing title memory, and before submissions the microseconds and bytes
/// of pre-GPU snapshots, since the last frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
        }
        text.push_str(&format!("; host caps: {}", host.caps.summary()));
        let heaps = crate::descriptor::shader_heap_stats();
        if !heaps.is_empty() {
            text.push_str(&format!("; descriptor heaps: {heaps}"));
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => {
                text.push_str("; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command")
            }
            None => {}
        }
    }
    tracing::error!("{text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(made) => {
            crate::memstat::start_monitor(
                video_memory,
                crate::shadow::relieve_pressure,
                stop_for_memory,
            );
            Some(made)
        }
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.into_iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        let warp =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some();
        if warp {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let caps = HostCaps::query(&device, warp);
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        name(&queue, || "victory direct queue".to_owned());
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        tracing::info!(caps = %caps.summary(), "host D3D12 device ready");
        Ok(Host {
            device,
            caps,
            format_support: Mutex::new(HashMap::new()),
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
        })
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by title resource: at its
    /// start and at its end. Lists are recorded on many threads and run in
    /// submission order, so the transitions from a texture's real state are
    /// made at submission, in `prelude`.
    states: Mutex<std::collections::HashMap<usize, (u32, u32)>>,
    /// The list that runs those transitions just before this one.
    prelude: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// Incremented whenever a deferred title-memory copy or write is recorded,
    /// so cached fresh descriptor tables in `draw` do not reuse pre-copy CBVs.
    copy_epoch: AtomicU64,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts).
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<String>>,
}

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some()
    })
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// Upload chunks handed out front to back: the chunks, the one in use and
/// the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(ID3D12Resource, usize, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

const SCRATCH_CHUNK: u64 = 4 << 20;

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory, with optional
    /// zero-padding up to `padded` bytes (for 256-byte aligned CBV tails).
    Fill(usize, usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a title texture's host copy (title resource, host texture) up
    /// to date if its memory changed: in the submission's prelude.
    Texture(
        std::sync::Arc<crate::texture::Staging>,
        usize,
        ID3D12Resource,
    ),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// Host COM objects referenced by recorded commands (pipelines, root
    /// signatures, descriptor heaps, textures), held alive until list reset.
    kept: Vec<windows::core::IUnknown>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both).
    pub(crate) fn mesh_shaders(&self) -> bool {
        self.caps.supports_mesh_shaders()
    }

    /// Cached `D3D12_FEATURE_FORMAT_SUPPORT` query for `format`.
    #[allow(dead_code)]
    pub(crate) fn format_support(
        &self,
        format: DXGI_FORMAT,
    ) -> (D3D12_FORMAT_SUPPORT1, D3D12_FORMAT_SUPPORT2) {
        let mut cache = self
            .format_support
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *cache.entry(format.0).or_insert_with(|| {
            let mut data = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
                Format: format,
                ..Default::default()
            };
            // SAFETY: CheckFeatureSupport writes only into `data`.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_FORMAT_SUPPORT,
                        (&raw mut data).cast(),
                        size_of_val(&data) as u32,
                    )
                    .is_ok()
            };
            if ok {
                (data.Support1, data.Support2)
            } else {
                (D3D12_FORMAT_SUPPORT1_NONE, D3D12_FORMAT_SUPPORT2_NONE)
            }
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        // SAFETY: plain object creation.
        unsafe {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            name(&list, || format!("victory list #{id}"));
            let prelude_allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let prelude: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &prelude_allocator,
                None,
            )?;
            name(&prelude, || format!("victory prelude #{id}"));
            prelude.Close()?;
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                prelude: (prelude_allocator, prelude),
                copy_epoch: AtomicU64::new(0),
                pipelines: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
            })
        }
    }

    /// Runs closed lists and waits for them.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        let mut batch = Batch::default();
        // Texture contents uploaded by the preludes: their upload buffers go
        // back to the pool once the submission has run.
        let mut uploads: Vec<crate::texture::Upload> = Vec::new();
        for list in runs {
            let mine = list.run_deferred(&mut batch);
            if let Some(prelude) = list.prelude(&mine) {
                lists.push(Some(prelude));
            }
            uploads.extend(mine);
            lists.push(list.list.cast().ok());
        }
        batch.flush();
        // Deduplicate shader-written ranges per shadow across all lists in
        // this submission, and snapshot pre-GPU bytes before executing so
        // copy-back writes back only what the GPU actually changed.
        type ShadowRanges = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);
        let mut gpu_writes: Vec<ShadowRanges> = Vec::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step {
                    if let Some((_, ranges)) = gpu_writes
                        .iter_mut()
                        .find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow))
                    {
                        if !ranges.contains(&(*address, *len)) {
                            ranges.push((*address, *len));
                        }
                    } else {
                        gpu_writes.push((shadow.clone(), vec![(*address, *len)]));
                    }
                }
            }
        }
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        // Snapshots are this submission's own and taken under `submitted`,
        // after every earlier submission's GPU work and copy-back: an
        // overlapping submission can neither replace nor consume them.
        let snapshotting = std::time::Instant::now();
        let snapshots: Vec<_> = gpu_writes
            .iter()
            .map(|(shadow, ranges)| shadow.snapshot_ranges(ranges))
            .collect();
        if !gpu_writes.is_empty() {
            let bytes: usize = snapshots.iter().flatten().map(|s| s.len()).sum();
            COPY_BACK[3].fetch_add(snapshotting.elapsed().as_micros() as u64, Ordering::Relaxed);
            COPY_BACK[4].fetch_add(bytes as u64, Ordering::Relaxed);
        }
        *submitted += 1;
        let prepared = std::time::Instant::now();
        // SAFETY: closed lists; the fence and event are ours.
        let signalled = unsafe {
            self.queue.ExecuteCommandLists(&lists);
            self.queue.Signal(&self.fence, *submitted)
        };
        if let Err(e) = &signalled {
            tracing::warn!(error = %e, "host submission not signalled");
        }
        let completed = wait_fence(
            &self.fence,
            self.event,
            *submitted,
            signalled.is_ok(),
            INFINITE,
            UNCONFIRMED_POLL,
        );
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            tracing::warn!(
                ms = gpu.as_millis(),
                lists = runs.len(),
                ?pipelines,
                "slow host submission"
            );
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        let copying_back = std::time::Instant::now();
        let (mut changed_bytes, mut write_us) = (0u64, 0u64);
        if completed {
            for ((shadow, ranges), snapshot) in gpu_writes.iter().zip(&snapshots) {
                let (changed, us) = shadow.copy_back_timed(ranges, snapshot.as_ref());
                changed_bytes += changed as u64;
                write_us += us;
            }
        } else {
            // The GPU may still be writing the copies: reading them now
            // would publish partial results into title memory. Drop this
            // submission's shader writes instead, and let later refreshes
            // copy title memory again rather than block on them forever.
            let n = UNCONFIRMED_WAITS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                value = *submitted,
                unconfirmed = n,
                shadows = gpu_writes.len(),
                "host submission completion not confirmed; shader writes not copied back"
            );
            for (shadow, ranges) in &gpu_writes {
                shadow.release(ranges);
            }
        }
        drop(snapshots);
        if !gpu_writes.is_empty() {
            COPY_BACK[0].fetch_add(copying_back.elapsed().as_micros() as u64, Ordering::Relaxed);
            COPY_BACK[1].fetch_add(changed_bytes, Ordering::Relaxed);
            COPY_BACK[2].fetch_add(write_us, Ordering::Relaxed);
        }
        // The preludes' upload buffers are free again.
        drop(uploads);
        for list in runs {
            let deferred = list.deferred();
            // After GPU copy-backs, write bottom-of-pipe markers so the title
            // sees completed GPU writes before observing completion.
            for &(at, what, bytes) in &deferred.writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            let mut fault_reported = false;
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                fault_reported = true;
                let va = fault.PageFaultVA;
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
            }
            if !fault_reported
                && let Ok(fault) = dred.GetPageFaultAllocationOutput()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let Ok(crumbs) = dred.GetAutoBreadcrumbsOutput() else {
                return text;
            };
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    let list_name = if p.pCommandListDebugNameW.is_null() {
                        "unnamed".to_owned()
                    } else {
                        p.pCommandListDebugNameW.to_string().unwrap_or_default()
                    };
                    let op = history[done as usize];
                    text.push_str(&format!(
                        "; list \"{list_name}\" stopped at command {done} of {} ({} [op {}])",
                        p.BreadcrumbCount,
                        breadcrumb_op_name(op),
                        op.0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), note)| (at as u32, note.clone()))
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    pub(crate) fn close(&self) {
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        *self.deferred() = Deferred::default();
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
    }

    /// Keeps a host COM object referenced by this list alive until reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(unk) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(unk);
        }
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(resource as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted));
                drop(states);
                self.keep(texture);
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with, copying `uploads`
    /// into their textures on the way (through the copy destination state);
    /// the textures' real states become those the list ends with.
    fn prelude(&self, uploads: &[crate::texture::Upload]) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let uploading = |resource: usize| uploads.iter().any(|u| u.resource == resource);
        let barrier = |texture: &ID3D12Resource, from: u32, to: u32| {
            transition(
                texture,
                D3D12_RESOURCE_STATES(from as i32),
                D3D12_RESOURCE_STATES(to as i32),
            )
        };
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (&resource, &(first, last)) in states.iter() {
            let this = resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(this) else {
                continue;
            };
            let real = crate::resource::swap_host_state(this, last);
            if uploading(resource) {
                if real != COPY_DEST {
                    before.push(barrier(texture, real, COPY_DEST));
                }
                if first != COPY_DEST {
                    after.push(barrier(texture, COPY_DEST, first));
                }
            } else if real != first {
                before.push(barrier(texture, real, first));
            }
        }
        // Uploaded textures the list does not move: back where they were.
        for upload in uploads.iter().filter(|u| !states.contains_key(&u.resource)) {
            let real = crate::resource::host_state(upload.resource as crate::object::This);
            if real != COPY_DEST {
                before.push(barrier(upload.texture(), real, COPY_DEST));
                after.push(barrier(upload.texture(), COPY_DEST, real));
            }
        }
        if before.is_empty() && after.is_empty() && uploads.is_empty() {
            return None;
        }
        let (allocator, list) = &self.prelude;
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures and upload buffers are live until it has run.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !before.is_empty() {
                    list.ResourceBarrier(&before);
                }
                for upload in uploads {
                    upload.record(list);
                }
                if !after.is_empty() {
                    list.ResourceBarrier(&after);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        if dred_on() {
            let what = what();
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list
                    .SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(what);
        }
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Monotonic counter bumped whenever this list records a title-memory copy
    /// or deferred CPU write.
    pub(crate) fn copy_epoch(&self) -> u64 {
        self.copy_epoch.load(Ordering::Relaxed)
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
        self.copy_epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.defer_fill_padded(dst, src, len, len);
    }

    /// Copies `len` bytes of title memory at `src` into upload memory at `dst`
    /// and zero-fills `[len, padded)` so reused upload chunks never expose
    /// stale tail bytes to constant buffer views.
    pub(crate) fn defer_fill_padded(
        &self,
        dst: *mut u8,
        src: *const u8,
        len: usize,
        padded: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len, padded.max(len)));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        track(&buffer, crate::memstat::Kind::ListUpload, size);
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            track(&buffer, crate::memstat::Kind::ListUpload, len);
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().uploads.push(buffer);
            return Some((gpu, data));
        }
        if s.used + len > SCRATCH_CHUNK && s.current < s.chunks.len() {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let (buffer, data) = host.upload_buffer(SCRATCH_CHUNK)?;
            track(&buffer, crate::memstat::Kind::ListUpload, SCRATCH_CHUNK);
            name(&buffer, || "list scratch".into());
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            s.chunks.push((buffer, data as usize, gpu));
            s.used = 0;
        }
        let (_, data, gpu) = s.chunks[s.current];
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Run(Box::new(work)));
        self.deferred().refreshed.clear();
        self.copy_epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// At submission, brings title texture `resource`'s host copy
    /// (`texture`) up to date if its memory changed (`staging`).
    pub(crate) fn defer_texture(
        &self,
        staging: std::sync::Arc<crate::texture::Staging>,
        resource: usize,
        texture: ID3D12Resource,
    ) {
        self.deferred()
            .copies
            .push(Step::Texture(staging, resource, texture));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    /// Returns the texture contents to upload in the list's prelude.
    fn run_deferred(&self, batch: &mut Batch) -> Vec<crate::texture::Upload> {
        let mut uploads = Vec::new();
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        for step in &self.deferred().copies {
            let started = std::time::Instant::now();
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    batch.flush();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    len
                }
                &Step::Fill(dst, src, len, padded) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    if len > 0 {
                        copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    }
                    if padded > len {
                        // SAFETY: `dst` points into our live mapped upload buffer
                        // allocated with at least `padded` bytes.
                        unsafe {
                            std::ptr::write_bytes((dst as *mut u8).add(len), 0, padded - len);
                        }
                    }
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.flush();
                    }
                    work();
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data.
                    batch.flush();
                    shadow.writing(*address, *len);
                    continue;
                }
                Step::Texture(staging, resource, texture) => {
                    uploads.extend(staging.refresh(*resource, texture));
                    continue;
                }
            };
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
        uploads
    }
}

/// Requested bytes of a batch past which its refreshes share threads.
/// shortcut: not measured; tune from an authorized run's preparing times.
const PARALLEL_REFRESH: usize = 8 << 20;

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
#[derive(Default)]
struct Batch(Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>);

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        match self
            .0
            .iter_mut()
            .find(|(s, ..)| std::sync::Arc::ptr_eq(s, shadow))
        {
            Some((_, from, to)) => (*from, *to) = ((*from).min(address), (*to).max(end)),
            None => self.0.push((shadow.clone(), address, end)),
        }
    }

    /// Brings the batched ranges up to date. Each entry is a distinct copy
    /// (its own host buffer), so big batches are shared among a few
    /// threads ([`crate::par`]); small ones run here, as before.
    fn flush(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        // Biggest first, for an even share among threads.
        self.0
            .sort_unstable_by_key(|&(_, from, to)| std::cmp::Reverse(to - from));
        let copied = AtomicU64::new(0);
        crate::par::for_each(
            &self.0,
            crate::par::workers(),
            PARALLEL_REFRESH,
            |&(_, from, to)| to - from,
            |(shadow, from, to)| {
                #[cfg(feature = "diagnostics")]
                let range_started = std::time::Instant::now();
                let bytes = shadow.update(*from, to - from);
                copied.fetch_add(bytes as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::range(
                    *from,
                    to - from,
                    range_started.elapsed().as_micros() as u64,
                    0,
                );
            },
        );
        self.0.clear();
        PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied.into_inner(), Ordering::Relaxed);
    }
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 || title_dst && !crate::texture::committed(dst, len) {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::MoveUs,
        moved.elapsed().as_micros() as u64,
    );
    if whole {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let mut done = 0;
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
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
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    hwnd: HWND,
    meter: Meter,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    logged_frames: u32,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            logged_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        self.frames += 1;
        self.logged_frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] =
            [0, 1].map(|i| SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [cb_ms, cb_mb, cb_write_ms, snap_ms, snap_mb] = [0, 1, 2, 3, 4].map(|i| {
            let scale = if i == 1 || i == 4 { 1e6 } else { 1e3 };
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            let log_frames = f64::from(self.logged_frames.max(1));
            self.logged_frames = 0;
            self.logged = std::time::Instant::now();
            let c = crate::draw::take_counts();
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; copy-back {cb_ms:.1} ms {cb_mb:.1} MB, of it writes {cb_write_ms:.1} ms; \
                 snapshots {snap_ms:.1} ms {snap_mb:.1} MB), {draws:.0} draws/frame \
                 [exec/req per frame: draw {:.0}/{:.0}, mesh {:.0}/{:.0}, dispatch {:.0}/{:.0}, \
                 dropped {:.0} (no_pipe={:.0}, bind={:.0}, other={:.0})]",
                c.draws_ok as f64 / log_frames,
                c.draws_req as f64 / log_frames,
                c.mesh_ok as f64 / log_frames,
                c.mesh_req as f64 / log_frames,
                c.disp_ok as f64 / log_frames,
                c.disp_req as f64 / log_frames,
                c.total_dropped() as f64 / log_frames,
                c.drop_no_pipeline as f64 / log_frames,
                c.drop_bind as f64 / log_frames,
                c.drop_other as f64 / log_frames,
            );
            #[cfg(feature = "diagnostics")]
            crate::copystat::log(log_frames);
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => (p.width, p.height, p.format) = (width, height, format),
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        let need_src_transition = state != D3D12_RESOURCE_STATE_COPY_SOURCE;
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            p.copy.reset();
            let list = &p.copy.list;
            let mut before = Vec::with_capacity(2);
            if need_src_transition {
                before.push(transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
            }
            before.push(transition(
                &back,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ));
            list.ResourceBarrier(&before);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = crate::capture::probe(&self.device, list, texture);
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            let mut after = Vec::with_capacity(2);
            if need_src_transition {
                after.push(transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
            }
            after.push(transition(
                &back,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PRESENT,
            ));
            list.ResourceBarrier(&after);
            p.copy.close();
            self.execute(&[&p.copy]);
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                hwnd,
                meter: Meter::new(),
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// The host GPU's memory use and budget (local, then non-local), from
    /// DXGI; `None` if the adapter cannot say.
    pub(crate) fn video_memory(&self) -> Option<crate::memstat::Video> {
        struct Adapter(IDXGIAdapter3);
        // SAFETY: DXGI adapters are free-threaded.
        unsafe impl Send for Adapter {}
        // SAFETY: as above.
        unsafe impl Sync for Adapter {}
        static ADAPTER: OnceLock<Option<Adapter>> = OnceLock::new();
        let adapter = ADAPTER.get_or_init(|| {
            // SAFETY: plain queries of a live device and DXGI.
            unsafe {
                let luid = self.device.GetAdapterLuid();
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                factory
                    .EnumAdapterByLuid::<IDXGIAdapter3>(luid)
                    .ok()
                    .map(Adapter)
            }
        });
        let adapter = &adapter.as_ref()?.0;
        let query = |group| {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            // SAFETY: node 0 of a live adapter and an output structure.
            unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }.ok()?;
            Some((info.CurrentUsage, info.Budget))
        };
        Some([
            query(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?,
            query(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?,
        ])
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for n in notes.iter().rev().take(6).rev() {
                text.push_str(&format!(
                    "; list {i} of {}, of its {}: {n}",
                    runs.len(),
                    notes.len()
                ));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Human-readable name for a D3D12 auto-breadcrumb opcode.
pub(crate) fn breadcrumb_op_name(op: D3D12_AUTO_BREADCRUMB_OP) -> &'static str {
    match op {
        D3D12_AUTO_BREADCRUMB_OP_SETMARKER => "SetMarker",
        D3D12_AUTO_BREADCRUMB_OP_BEGINEVENT => "BeginEvent",
        D3D12_AUTO_BREADCRUMB_OP_ENDEVENT => "EndEvent",
        D3D12_AUTO_BREADCRUMB_OP_DRAWINSTANCED => "DrawInstanced",
        D3D12_AUTO_BREADCRUMB_OP_DRAWINDEXEDINSTANCED => "DrawIndexedInstanced",
        D3D12_AUTO_BREADCRUMB_OP_EXECUTEINDIRECT => "ExecuteIndirect",
        D3D12_AUTO_BREADCRUMB_OP_DISPATCH => "Dispatch",
        D3D12_AUTO_BREADCRUMB_OP_COPYBUFFERREGION => "CopyBufferRegion",
        D3D12_AUTO_BREADCRUMB_OP_COPYTEXTUREREGION => "CopyTextureRegion",
        D3D12_AUTO_BREADCRUMB_OP_COPYRESOURCE => "CopyResource",
        D3D12_AUTO_BREADCRUMB_OP_COPYTILES => "CopyTiles",
        D3D12_AUTO_BREADCRUMB_OP_RESOLVESUBRESOURCE => "ResolveSubresource",
        D3D12_AUTO_BREADCRUMB_OP_CLEARRENDERTARGETVIEW => "ClearRenderTargetView",
        D3D12_AUTO_BREADCRUMB_OP_CLEARUNORDEREDACCESSVIEW => "ClearUnorderedAccessView",
        D3D12_AUTO_BREADCRUMB_OP_CLEARDEPTHSTENCILVIEW => "ClearDepthStencilView",
        D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER => "ResourceBarrier",
        D3D12_AUTO_BREADCRUMB_OP_EXECUTEBUNDLE => "ExecuteBundle",
        D3D12_AUTO_BREADCRUMB_OP_PRESENT => "Present",
        D3D12_AUTO_BREADCRUMB_OP_RESOLVEQUERYDATA => "ResolveQueryData",
        D3D12_AUTO_BREADCRUMB_OP_BEGINSUBMISSION => "BeginSubmission",
        D3D12_AUTO_BREADCRUMB_OP_ENDSUBMISSION => "EndSubmission",
        D3D12_AUTO_BREADCRUMB_OP_DISPATCHRAYS => "DispatchRays",
        D3D12_AUTO_BREADCRUMB_OP_BUILDRAYTRACINGACCELERATIONSTRUCTURE => {
            "BuildRaytracingAccelerationStructure"
        }
        D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH => "DispatchMesh",
        _ => "UnknownOp",
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(
    history: &[D3D12_AUTO_BREADCRUMB_OP],
    markers: &[(u32, String)],
    done: usize,
) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done]
        .iter()
        .rposition(barrier)
        .map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    for i in from..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!(
            "; command {i} ({} [op {}]): {marker}",
            breadcrumb_op_name(op),
            op.0
        ));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
pub(crate) fn warp_device() -> Option<ID3D12Device> {
    // SAFETY: creates an isolated WARP software adapter device for deterministic tests.
    unsafe {
        if std::env::var_os("VICTORY_D3D12_DEBUG").is_some() {
            let mut layer: Option<ID3D12Debug> = None;
            if D3D12GetDebugInterface(&mut layer).is_ok()
                && let Some(l) = layer
            {
                l.EnableDebugLayer();
            }
        }
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
        device
    }
}

#[cfg(test)]
pub(crate) fn assert_no_debug_errors(device: &ID3D12Device) {
    let Ok(queue) = device.cast::<ID3D12InfoQueue>() else {
        return;
    };
    let mut errors = Vec::new();
    // SAFETY: reads stored D3D12 debug-layer messages into a sized buffer.
    unsafe {
        for i in 0..queue.GetNumStoredMessages() {
            let mut size = 0;
            if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                continue;
            }
            let mut buffer = vec![0u64; size.div_ceil(8)];
            let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                let m = &*message;
                if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                    let bytes = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(bytes)
                        .trim_end_matches('\0')
                        .to_owned();
                    errors.push(format!("[id={} sev={}] {text}", m.ID.0, m.Severity.0));
                }
            }
        }
        queue.ClearStoredMessages();
    }
    assert!(
        errors.is_empty(),
        "D3D12 Debug Layer reported errors:\n{}",
        errors.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_memory_reports_a_local_budget() {
        let Some(host) = host() else {
            return;
        };
        let [(_, local_budget), _] = host.video_memory().expect("DXGI 1.4 adapter");
        assert!(local_budget > 0);
    }

    /// Completion is confirmed only when the fence really reached the
    /// value: not on a timed-out or never-signalled wait, nor on a removed
    /// device's `u64::MAX`.
    #[test]
    fn fence_waits_confirm_only_real_completion() {
        let Some(host) = host() else {
            return;
        };
        let short = std::time::Duration::from_millis(20);
        // SAFETY: a fresh fence and event of ours.
        let (fence, event): (ID3D12Fence, HANDLE) = unsafe {
            (
                host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                CreateEventW(None, false, false, None).unwrap(),
            )
        };
        assert!(
            !wait_fence(&fence, event, 1, true, 10, short),
            "not reached"
        );
        assert!(
            !wait_fence(&fence, event, 1, false, 10, short),
            "never signalled"
        );
        // SAFETY: a CPU-side signal of our fence.
        unsafe { fence.Signal(1).unwrap() };
        assert!(wait_fence(&fence, event, 1, true, 10, short));
        assert!(
            wait_fence(&fence, event, 1, false, 10, short),
            "already done"
        );
        // Arming for the reached value 1 leaves the shared event set; that
        // stale wake must not count as completion of a later value.
        // SAFETY: our fence.
        unsafe { fence.SetEventOnCompletion(1, event).unwrap() };
        assert!(!wait_fence(&fence, event, 2, true, 10, short), "stale wake");
        // SAFETY: as above; `u64::MAX` is what a removed device reports.
        unsafe { fence.Signal(u64::MAX).unwrap() };
        assert!(
            !wait_fence(&fence, event, 3, true, 10, short),
            "device removed"
        );
        // SAFETY: our event.
        unsafe { windows::Win32::Foundation::CloseHandle(event).unwrap() };
    }

    fn synthetic_caps(
        sm: D3D_SHADER_MODEL,
        wave_ops: bool,
        wave_min: u32,
        wave_max: u32,
        mesh_tier: D3D12_MESH_SHADER_TIER,
    ) -> HostCaps {
        HostCaps {
            adapter_name: "Synthetic Test GPU".to_owned(),
            vendor_id: 0x10de,
            device_id: 0x2484,
            dedicated_vram: 8u64 << 30,
            warp: false,
            highest_shader_model: sm,
            resource_binding_tier: D3D12_RESOURCE_BINDING_TIER_3,
            resource_heap_tier: D3D12_RESOURCE_HEAP_TIER_2,
            typed_uav_load_additional_formats: true,
            wave_ops,
            wave_lane_count_min: wave_min,
            wave_lane_count_max: wave_max,
            mesh_shader_tier: mesh_tier,
            enhanced_barriers: true,
            max_gpu_va_bits_per_resource: 40,
            max_gpu_va_bits_per_process: 40,
            uma: false,
            existing_heaps: false,
        }
    }

    #[test]
    fn host_caps_wave_and_mesh_support_checks() {
        // Wave32-only GPU (e.g. NVIDIA Ampere).
        let wave32_only =
            synthetic_caps(D3D_SHADER_MODEL_6_6, true, 32, 32, D3D12_MESH_SHADER_TIER_1);
        assert!(wave32_only.supports_shader_model_6_6());
        assert!(wave32_only.supports_mesh_shaders());
        assert!(wave32_only.supports_wave_size(None));
        assert!(wave32_only.supports_wave_size(Some(32)));
        assert!(!wave32_only.supports_wave_size(Some(64)));
        assert!(wave32_only.summary().contains("wave=32..32(ops=true)"));

        // Wave32..64 GPU (e.g. AMD RDNA2).
        let dual_wave =
            synthetic_caps(D3D_SHADER_MODEL_6_7, true, 32, 64, D3D12_MESH_SHADER_TIER_1);
        assert!(dual_wave.supports_wave_size(Some(32)));
        assert!(dual_wave.supports_wave_size(Some(64)));

        // GPU lacking SM 6.6 or mesh shaders or wave ops.
        let legacy = synthetic_caps(
            D3D_SHADER_MODEL_6_5,
            false,
            32,
            32,
            D3D12_MESH_SHADER_TIER_NOT_SUPPORTED,
        );
        assert!(!legacy.supports_shader_model_6_6());
        assert!(!legacy.supports_mesh_shaders());
        assert!(legacy.supports_wave_size(None));
        assert!(!legacy.supports_wave_size(Some(32)));
    }

    #[test]
    fn warp_caps_query_succeeds() {
        let Some(device) = warp_device() else {
            return;
        };
        let caps = HostCaps::query(&device, true);
        assert!(caps.warp);
        assert!(caps.supports_shader_model_6_6());
        assert!(caps.summary().contains("warp=true"));
    }

    #[test]
    fn dred_breadcrumb_names_and_in_flight_formatting() {
        assert_eq!(
            breadcrumb_op_name(D3D12_AUTO_BREADCRUMB_OP_DRAWINDEXEDINSTANCED),
            "DrawIndexedInstanced"
        );
        assert_eq!(
            breadcrumb_op_name(D3D12_AUTO_BREADCRUMB_OP_DISPATCH),
            "Dispatch"
        );
        assert_eq!(
            breadcrumb_op_name(D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH),
            "DispatchMesh"
        );
        assert_eq!(
            breadcrumb_op_name(D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER),
            "ResourceBarrier"
        );

        let history = [
            D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER,
            D3D12_AUTO_BREADCRUMB_OP_SETMARKER,
            D3D12_AUTO_BREADCRUMB_OP_DRAWINDEXEDINSTANCED,
            D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER,
        ];
        let markers = [(1, "indexed 36x1 [vertex vs, pixel ps]".to_owned())];
        let report = in_flight(&history, &markers, 2);
        assert!(report.contains("commands 1..3 between its barriers"));
        assert!(report.contains("DrawIndexedInstanced [op 4]"));
        assert!(report.contains("indexed 36x1 [vertex vs, pixel ps]"));
    }
}
