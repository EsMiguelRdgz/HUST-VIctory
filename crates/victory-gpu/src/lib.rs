//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
pub mod fence;
pub mod frame;
#[cfg(windows)]
mod host;
pub mod list;
pub mod memstat;
pub mod object;
mod par;
pub mod pipeline;
#[cfg(windows)]
mod primitive;
pub mod query;
pub mod queue;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shadow;
pub mod signature;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
pub use host::on_failure;

#[cfg(windows)]
static COMMIT_RANGE: std::sync::OnceLock<fn(usize, usize)> = std::sync::OnceLock::new();

/// Registers the runtime's callback (`xmem::ensure_committed`) that commits
/// any demand-mapped guest physical pages intersecting `[at, at + len)`.
#[cfg(windows)]
pub fn use_commit_range(commit: fn(usize, usize)) {
    let _ = COMMIT_RANGE.set(commit);
}

/// Ensures any demand-mapped guest physical pages in `[at, at + len)` are
/// committed before a kernel or GPU-writeback operation writes into them.
#[cfg(windows)]
pub(crate) fn ensure_title_committed(at: usize, len: usize) {
    if len > 0
        && let Some(commit) = COMMIT_RANGE.get()
    {
        commit(at, len);
    }
}

/// Called before the host kernel writes into `[at, at + len)` (`ReadFile`,
/// `fread`, DirectStorage memory reads): ensures demand-mapped physical pages
/// are committed and removes write-watch read-only guards.
#[cfg(windows)]
pub fn kernel_will_write(at: usize, len: usize) {
    ensure_title_committed(at, len);
    watch::writing(at, len);
}

/// Called by the runtime (`xmem`) when title physical/virtual mappings in
/// `[at, at + len)` change: updates write-watch guard protections and evicts
/// any cached placed host textures covering the remapped range.
#[cfg(windows)]
pub fn title_memory_remapped(at: usize, len: usize) {
    watch::title_memory_remapped(at, len);
    resource::on_title_memory_remapped(at as u64, len as u64);
}
