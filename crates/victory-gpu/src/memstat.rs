//! Where memory goes (Phase 11): live and peak bytes of every large
//! allocation category this runtime makes or manages, with the process's
//! private commit and working set, the system's commit and available
//! physical memory, the host GPU's budgets, and a breakdown of the
//! process's address space, so that commit growth in an authorized run can
//! be attributed instead of guessed at.
//!
//! These measure different things and must not be mixed up:
//! - *reserved* address space costs neither memory nor commit;
//! - *committed* memory (private allocations, pagefile-backed section pages
//!   once committed) is charged against the system commit limit whether or
//!   not it was ever touched;
//! - the *working set* is the committed memory resident in RAM right now;
//! - *private bytes* are the process's private commit; committed pages of
//!   pagefile-backed sections (the title's physical pages) are charged to
//!   the system commit but are not private bytes;
//! - GPU *local* memory is video memory; *non-local* is system memory the
//!   GPU uses (resources placed or demoted there).
//!
//! Counting is a few relaxed atomic adds per allocation and free. D3D12
//! objects are counted for their whole life by [`track`], which hangs a
//! small object on them that the runtime releases when they are destroyed.
//! Every 5 s [`summary`] is logged; with `VICTORY_MEMORY_REPORT_DIR` set the
//! same lines also go, timestamped, to a file there ([`report`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What a counted allocation is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Host copies of title buffer objects (system memory the GPU reads).
    Buffer,
    /// Host copies of aligned windows of title memory outside buffers.
    Loose,
    /// Dedicated copies for buffer views whose offset needs realigning.
    Aligned,
    /// Pre-GPU snapshots held by submissions in flight.
    Snapshot,
    /// Snapshot buffers kept for reuse.
    SnapshotPool,
    /// Upload buffers title-written textures are detiled into.
    TextureStaging,
    /// Buffers texture memory is read into before detiling (pooled).
    TextureRead,
    /// Host textures for title textures (video memory, or system memory
    /// when the GPU's budget is exceeded), sized by the host's layout.
    HostTexture,
    /// Title textures whose host D3D12 texture is deferred until first GPU
    /// use (estimated footprint; consumes 0 bytes of host VRAM/commit).
    DeferredTexture,
    /// Upload memory lists hold until reset (scratch, uploads).
    ListUpload,
    /// Host descriptor heaps.
    DescriptorHeap,
    /// Title heaps and committed resources whose memory this runtime
    /// allocated itself (not through the title's allocator).
    TitleHeap,
    /// Committed pages of the title's physical-page sections (charged to
    /// the system commit, not to private bytes).
    GuestPhysical,
}

const KINDS: usize = 13;
const ALL: [(Kind, &str); KINDS] = [
    (Kind::Buffer, "buffer copies"),
    (Kind::Loose, "loose copies"),
    (Kind::Aligned, "aligned copies"),
    (Kind::Snapshot, "snapshots"),
    (Kind::SnapshotPool, "snapshot pool"),
    (Kind::TextureStaging, "texture staging"),
    (Kind::TextureRead, "texture reads"),
    (Kind::HostTexture, "host textures"),
    (Kind::DeferredTexture, "deferred textures"),
    (Kind::ListUpload, "list uploads"),
    (Kind::DescriptorHeap, "descriptor heaps"),
    (Kind::TitleHeap, "title heaps"),
    (Kind::GuestPhysical, "guest physical"),
];

/// Categories whose committed bytes live inside the process's `PrivateUsage`
/// (excluding `DeferredTexture`, which holds no host commit, and
/// `GuestPhysical`, which is `MEM_MAPPED` pagefile-backed sections).
const PRIVATE_KINDS: [Kind; 11] = [
    Kind::Buffer,
    Kind::Loose,
    Kind::Aligned,
    Kind::Snapshot,
    Kind::SnapshotPool,
    Kind::TextureStaging,
    Kind::TextureRead,
    Kind::HostTexture,
    Kind::ListUpload,
    Kind::DescriptorHeap,
    Kind::TitleHeap,
];

/// Live bytes, live allocations, peak live bytes, allocations ever made.
struct Counter([AtomicU64; 4]);

impl Counter {
    const fn new() -> Counter {
        Counter([const { AtomicU64::new(0) }; 4])
    }

    fn alloc(&self, bytes: u64) {
        let [live, count, peak, made] = &self.0;
        let now = live.fetch_add(bytes, Relaxed) + bytes;
        count.fetch_add(1, Relaxed);
        peak.fetch_max(now, Relaxed);
        made.fetch_add(1, Relaxed);
    }

    fn free(&self, bytes: u64) {
        let [live, count, ..] = &self.0;
        // Saturating: never wrap on a miscount, which would hide real growth.
        let _ = live.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(bytes)));
        let _ = count.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)));
    }

    /// Bytes without a count of their own (pages of a counted section).
    fn adjust(&self, more: u64, less: u64) {
        let [live, _, peak, _] = &self.0;
        let now = live.fetch_add(more, Relaxed) + more;
        peak.fetch_max(now, Relaxed);
        let _ = live.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(less)));
    }

    fn get(&self) -> (u64, u64, u64) {
        let [live, count, peak, _] = &self.0;
        (live.load(Relaxed), count.load(Relaxed), peak.load(Relaxed))
    }

    fn made(&self) -> u64 {
        self.0[3].load(Relaxed)
    }
}

static LIVE: [Counter; KINDS] = [const { Counter::new() }; KINDS];

/// `bytes` more of `kind` are live.
pub fn alloc(kind: Kind, bytes: u64) {
    LIVE[kind as usize].alloc(bytes);
}

/// `bytes` of `kind` were freed (as counted by [`alloc`]).
pub fn free(kind: Kind, bytes: u64) {
    LIVE[kind as usize].free(bytes);
}

/// `more` bytes of `kind` became live and `less` stopped being, without a
/// change in how many allocations are live (pages committed in, or
/// released with, an allocation already counted).
pub fn adjust(kind: Kind, more: u64, less: u64) {
    LIVE[kind as usize].adjust(more, less);
}

/// Live bytes, live allocations and peak bytes of `kind`.
pub fn live(kind: Kind) -> (u64, u64, u64) {
    LIVE[kind as usize].get()
}

/// Allocations of `kind` ever counted.
pub fn made(kind: Kind) -> u64 {
    LIVE[kind as usize].made()
}

/// Events of the memory managers, counted for the report.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    /// Idle host copies freed by budget reclamation.
    Reclaimed,
    /// Bytes they held.
    ReclaimedBytes,
    /// Reclamation runs forced by memory pressure.
    PressureReclaims,
    /// Allocations refused or failed (host or title).
    Failures,
    /// Pooled buffer reuses (snapshots, texture reads, staging).
    PoolHits,
    /// Pool misses (a new buffer was allocated).
    PoolMisses,
    /// Deferred host textures materialized on first GPU use.
    LazyMaterializations,
    /// Idle placed or title-written host textures evicted to reclaim commit.
    TexturesEvicted,
    /// Bytes reclaimed by host texture eviction.
    TexturesEvictedBytes,
}

const EVENTS: usize = 9;
static EVENT: [AtomicU64; EVENTS] = [const { AtomicU64::new(0) }; EVENTS];

/// Counts `n` of `event`.
pub fn count(event: Event, n: u64) {
    EVENT[event as usize].fetch_add(n, Relaxed);
}

/// How many of `event` were counted.
pub fn events(event: Event) -> u64 {
    EVENT[event as usize].load(Relaxed)
}

/// Keeps `kind`'s count of `bytes` for as long as `object` lives: the
/// object (a D3D12 resource or heap) holds a small tracker, which the
/// runtime releases when the object is destroyed, uncounting the bytes.
pub(crate) fn track(
    object: &windows::Win32::Graphics::Direct3D12::ID3D12Object,
    kind: Kind,
    bytes: u64,
) {
    use windows::core::IUnknown;
    alloc(kind, bytes);
    let tracker: IUnknown = Tracker { kind, bytes }.into();
    // SAFETY: a live object; it takes a reference to the tracker.
    if unsafe { object.SetPrivateDataInterface(&TRACKER, &tracker) }.is_err() {
        // Not tracked: do not count what could never be uncounted.
        drop(tracker);
    }
}

/// The private data slot trackers use.
const TRACKER: windows::core::GUID =
    windows::core::GUID::from_u128(0x6f1c_2b54_9a1e_4c8b_b7d2_3e0a_5c47_91f3);

#[windows::core::implement()]
struct Tracker {
    kind: Kind,
    bytes: u64,
}

impl Drop for Tracker {
    fn drop(&mut self) {
        free(self.kind, self.bytes);
        #[cfg(test)]
        DROPPED.lock().unwrap().push(self.bytes);
    }
}

/// Sizes of trackers dropped, for tests.
#[cfg(test)]
static DROPPED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The process's private commit and working set, in bytes.
pub fn process() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a structure of the size given.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(),
            c.cb,
        )
    } != 0;
    ok.then_some((c.PrivateUsage as u64, c.WorkingSetSize as u64))
}

/// The system's committed bytes and commit limit.
pub fn system_commit() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetPerformanceInfo, PERFORMANCE_INFORMATION,
    };
    let mut info = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: a structure of the size given.
    let ok = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
    let page = info.PageSize as u64;
    ok.then_some((
        info.CommitTotal as u64 * page,
        info.CommitLimit as u64 * page,
    ))
}

/// Physical memory available and installed, in bytes.
pub fn physical() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a structure of the size given.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    ok.then_some((status.ullAvailPhys, status.ullTotalPhys))
}

/// The process's address space by state and type, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// Committed private memory (heaps, `VirtualAlloc` commits).
    pub private: u64,
    /// Committed pages of mapped views (sections; aliases count again).
    pub mapped: u64,
    /// Committed image pages (executables, DLLs).
    pub image: u64,
    /// Reserved, not committed.
    pub reserved: u64,
    /// Of `private` and `mapped`, what lies in title memory's address range
    /// (`[2^32, 2^47)`, where xmem places it).
    pub title: u64,
    /// Of `private`, what lies in title memory's address range.
    pub title_private: u64,
    /// Of `mapped`, what lies in title memory's address range (includes aliases).
    pub title_mapped: u64,
    /// Regions walked.
    pub regions: u64,
}

/// Walks the address space (tens of thousands of regions: milliseconds).
pub fn space() -> Space {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let mut space = Space::default();
    let mut at = 0usize;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let size = info.RegionSize as u64;
        let base = info.BaseAddress as u64;
        space.regions += 1;
        match info.State {
            MEM_COMMIT => {
                match info.Type {
                    MEM_IMAGE => space.image += size,
                    MEM_MAPPED => space.mapped += size,
                    _ => space.private += size,
                }
                if info.Type != MEM_IMAGE && (1 << 32..1 << 47).contains(&base) {
                    space.title += size;
                    if info.Type == MEM_MAPPED {
                        space.title_mapped += size;
                    } else {
                        space.title_private += size;
                    }
                }
            }
            MEM_RESERVE => space.reserved += size,
            _ => {}
        }
        let Some(next) = at.checked_add(info.RegionSize.max(4096)) else {
            break;
        };
        at = next;
    }
    space
}

/// The host GPU's memory use and budget, local (video memory) then
/// non-local (system memory the GPU uses), in bytes.
pub(crate) type Video = [(u64, u64); 2];

/// Commit headroom below which allocations count as under pressure: the
/// larger of 2 GiB and 5% of the limit.
pub fn pressure_floor(limit: u64) -> u64 {
    (2u64 << 30).max(limit / 20)
}

/// Whether committing `incoming` more bytes would leave the system less
/// than [`pressure_floor`] of commit headroom (false if it cannot tell).
pub fn under_pressure(incoming: u64) -> bool {
    system_commit().is_some_and(|(total, limit)| {
        limit.saturating_sub(total).saturating_sub(incoming) < pressure_floor(limit)
    })
}

/// Non-double-counted breakdown of VIctory's system-commit contribution.
///
/// `private` already includes all [`PRIVATE_KINDS`] (including WDDM-backed
/// `HostTexture`s and GPU `Buffer` copies), while `GuestPhysical` lives in
/// `MEM_MAPPED` pagefile-backed sections disjoint from `private`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitBreakdown {
    /// Sum of [`PRIVATE_KINDS`] live bytes (subset of `process_private`).
    pub tracked_private: u64,
    /// `process_private.saturating_sub(tracked_private)`: title private commit
    /// (`XMemVirtualAlloc`), heap/stacks, and driver overhead.
    pub untracked_private: u64,
    /// Committed guest physical section bytes (`Kind::GuestPhysical`).
    pub guest_physical: u64,
    /// Estimated VIctory total commit (`process_private + guest_physical`).
    pub victory_commit: u64,
    /// Estimated commit held by other processes (`system_commit - victory_commit`).
    pub other_commit: u64,
}

/// Computes the non-double-counted commit breakdown from live counters and
/// OS totals.
pub fn commit_breakdown(process_private: u64, system_commit: u64) -> CommitBreakdown {
    let tracked_private: u64 = PRIVATE_KINDS.iter().map(|&k| live(k).0).sum();
    let untracked_private = process_private.saturating_sub(tracked_private);
    let guest_physical = live(Kind::GuestPhysical).0;
    let victory_commit = process_private.saturating_add(guest_physical);
    let other_commit = system_commit.saturating_sub(victory_commit);
    CommitBreakdown {
        tracked_private,
        untracked_private,
        guest_physical,
        victory_commit,
        other_commit,
    }
}

/// One line: process, system and GPU memory, then every category.
pub(crate) fn summary(video: Option<Video>) -> String {
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    let mut text = String::from("host memory:");
    let proc_mem = process();
    let sys_mem = system_commit();
    if let Some((private, working)) = proc_mem {
        text.push_str(&format!(
            " private {:.0} MB, working set {:.0} MB;",
            mb(private),
            mb(working)
        ));
    }
    if let Some((total, limit)) = sys_mem {
        text.push_str(&format!(
            " system commit {:.0}/{:.0} MB;",
            mb(total),
            mb(limit)
        ));
    }
    if let (Some((private, _)), Some((total, _))) = (proc_mem, sys_mem) {
        let b = commit_breakdown(private, total);
        text.push_str(&format!(
            " victory est {:.0} MB (tracked private {:.0} + untracked private {:.0} + guest physical {:.0}), other {:.0} MB;",
            mb(b.victory_commit),
            mb(b.tracked_private),
            mb(b.untracked_private),
            mb(b.guest_physical),
            mb(b.other_commit),
        ));
    }
    if let Some((available, installed)) = physical() {
        text.push_str(&format!(
            " physical available {:.0}/{:.0} MB;",
            mb(available),
            mb(installed)
        ));
    }
    if let Some([(local, local_budget), (shared, shared_budget)]) = video {
        text.push_str(&format!(
            " GPU local {:.0}/{:.0} MB, non-local {:.0}/{:.0} MB;",
            mb(local),
            mb(local_budget),
            mb(shared),
            mb(shared_budget)
        ));
    }
    text.push_str(" categories:");
    for (kind, name) in ALL {
        let (bytes, count, peak) = live(kind);
        text.push_str(&format!(
            " {name} {count} = {:.1} MB (peak {:.1}),",
            mb(bytes),
            mb(peak)
        ));
    }
    text.pop();
    text.push_str(&format!(
        "; reclaimed {} ({:.1} MB), pressure reclaims {}, failures {}, pool hits/misses {}/{}, lazy mat {}, tex evicted {} ({:.1} MB)",
        events(Event::Reclaimed),
        mb(events(Event::ReclaimedBytes)),
        events(Event::PressureReclaims),
        events(Event::Failures),
        events(Event::PoolHits),
        events(Event::PoolMisses),
        events(Event::LazyMaterializations),
        events(Event::TexturesEvicted),
        mb(events(Event::TexturesEvictedBytes)),
    ));
    text
}

/// The address-space line (slower: logged less often).
pub fn space_summary() -> String {
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    let s = space();
    let guest_phys = live(Kind::GuestPhysical).0;
    let mapped_aliased = s.mapped.saturating_sub(guest_phys);
    format!(
        "address space: committed private {:.0} MB, mapped {:.0} MB (aliased ~{:.0} MB vs guest physical {:.0} MB), \
         image {:.0} MB; reserved {:.0} MB; in title range {:.0} MB (private {:.0}, mapped {:.0}); {} regions",
        mb(s.private),
        mb(s.mapped),
        mb(mapped_aliased),
        mb(guest_phys),
        mb(s.image),
        mb(s.reserved),
        mb(s.title),
        mb(s.title_private),
        mb(s.title_mapped),
        s.regions
    )
}

/// Commit headroom below which the run is in danger of taking the whole
/// system down with it: the larger of 1 GiB and 2% of the limit.
pub fn critical_floor(limit: u64) -> u64 {
    (1u64 << 30).max(limit / 50)
}

/// What the commit guard decides after a check of the headroom.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Guard {
    Fine,
    /// Below the critical floor this many checks in a row.
    Low(u32),
    /// Still low after relief: end the run.
    Stop,
}

/// One check of the guard: `low` checks in a row so far, whether this one is
/// below the floor, and whether relief was already tried in this episode.
/// Relief comes after 3 low checks in a row; the run stops only after 3
/// more, still low, so relief has time to show in the commit.
pub(crate) fn guard_step(low: u32, below: bool, relieved: bool) -> Guard {
    let n = low + 1;
    match below {
        false => Guard::Fine,
        true if relieved && n >= 6 => Guard::Stop,
        true => Guard::Low(n),
    }
}

/// Starts, once, the thread that logs [`summary`] every 5 s and
/// [`space_summary`] every 30 s (and reports both): independent of frames,
/// so a title that stops presenting under memory pressure is still seen.
/// It also guards the system: when commit headroom stays below
/// [`critical_floor`] for 3 s it calls `relieve`, and if that does not lift
/// it within 3 s more, `stop` with the memory report, ending the run with diagnostics
/// instead of letting the machine run out of commit (Windows becomes
/// unresponsive then). `VICTORY_MEMORY_GUARD=0` turns the guard off.
pub(crate) fn start_monitor(
    video: fn() -> Option<Video>,
    relieve: fn(u64) -> u64,
    stop: fn(&str) -> !,
) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let guarded = std::env::var_os("VICTORY_MEMORY_GUARD").is_none_or(|v| v != "0");
        let started = std::thread::Builder::new()
            .name("victory memory monitor".into())
            .spawn(move || {
                let (mut second, mut low, mut relieved) = (0u64, 0u32, false);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    second += 1;
                    if guarded && let Some((total, limit)) = system_commit() {
                        let headroom = limit.saturating_sub(total);
                        match guard_step(low, headroom < critical_floor(limit), relieved) {
                            Guard::Fine => (low, relieved) = (0, false),
                            Guard::Low(n) => {
                                low = n;
                                if n >= 3 && !relieved {
                                    let freed = relieve(0);
                                    relieved = true;
                                    tracing::error!(
                                        headroom,
                                        freed,
                                        "system commit nearly exhausted; host copies and pools freed"
                                    );
                                }
                            }
                            Guard::Stop => {
                                let text = format!(
                                    "the system is out of commit ({} MB left of {} MB) and                                      freeing VIctory's idle copies did not help; stopping before                                      Windows becomes unresponsive. {} {}",
                                    headroom >> 20,
                                    limit >> 20,
                                    summary(video()),
                                    space_summary()
                                );
                                report(&text);
                                stop(&text);
                            }
                        }
                    }
                    if second.is_multiple_of(5) {
                        let line = summary(video());
                        tracing::info!("{line}");
                        report(&line);
                    }
                    if second.is_multiple_of(30) {
                        let line = space_summary();
                        tracing::info!("{line}");
                        report(&line);
                    }
                }
            });
        if let Err(e) = started {
            tracing::warn!(error = %e, "no memory monitor");
        }
    });
}

/// The report file, opened on first use if `VICTORY_MEMORY_REPORT_DIR` is
/// set: `memory-<unix seconds>.log` there.
static REPORT: Mutex<Option<Option<std::fs::File>>> = Mutex::new(None);

/// Appends `line` to the report file (if one is configured), prefixed
/// with the Unix time in milliseconds.
pub fn report(line: &str) {
    use std::io::Write;
    let mut report = REPORT.lock().unwrap_or_else(|e| e.into_inner());
    let file = report.get_or_insert_with(|| {
        let dir = std::env::var_os("VICTORY_MEMORY_REPORT_DIR")?;
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).ok()?;
        let secs = now_ms() / 1000;
        let path = dir.join(format!("memory-{secs}.log"));
        let file = std::fs::File::create(&path).ok();
        tracing::info!(path = %path.display(), ok = file.is_some(), "memory report");
        file
    });
    if let Some(file) = file {
        let _ = writeln!(file, "{} {line}", now_ms());
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_track_live_bytes_and_peaks_without_wrapping() {
        let c = Counter::new();
        c.alloc(3 << 20);
        c.alloc(1 << 20);
        assert_eq!(c.get(), (4 << 20, 2, 4 << 20));
        c.free(3 << 20);
        assert_eq!(c.get(), (1 << 20, 1, 4 << 20), "peak kept");
        c.free(1 << 20);
        c.free(1 << 20);
        assert_eq!(c.get(), (0, 0, 4 << 20), "a miscount saturates");
        assert_eq!(c.made(), 2);
        c.adjust(5, 2);
        assert_eq!(c.get().0, 3);
    }

    #[test]
    fn process_and_system_memory_are_reported() {
        let (private, working) = process().expect("process counters");
        assert!(private > 0 && working > 0);
        let (total, limit) = system_commit().expect("performance information");
        assert!(total > 0 && limit >= total);
        let (available, installed) = physical().expect("memory status");
        assert!(available > 0 && installed >= available);
        let line = summary(Some([(1 << 20, 2 << 20), (0, 4 << 20)]));
        assert!(line.contains("private") && line.contains("GPU local 1/2 MB"));
        assert!(line.contains("snapshot pool") && line.contains("guest physical"));
        assert!(line.contains("deferred textures") && line.contains("victory est"));
        assert!(line.contains("pressure reclaims") && line.contains("lazy mat"));
    }

    #[test]
    fn commit_breakdown_avoids_double_counting_deferred_and_private_categories() {
        // DeferredTexture holds no real commit; it must not inflate tracked_private
        // or victory_commit.
        let before = commit_breakdown(1000 << 20, 2000 << 20);
        alloc(Kind::DeferredTexture, 64 << 20);
        let with_deferred = commit_breakdown(1000 << 20, 2000 << 20);
        free(Kind::DeferredTexture, 64 << 20);
        assert_eq!(with_deferred.tracked_private, before.tracked_private);
        assert_eq!(with_deferred.victory_commit, before.victory_commit);

        // HostTexture lives inside process_private, so tracked_private increases
        // while victory_commit (process_private + guest_physical) stays equal.
        alloc(Kind::HostTexture, 32 << 20);
        let with_host = commit_breakdown(1000 << 20, 2000 << 20);
        free(Kind::HostTexture, 32 << 20);
        assert_eq!(
            with_host
                .tracked_private
                .saturating_sub(before.tracked_private),
            32 << 20
        );
        assert_eq!(with_host.victory_commit, before.victory_commit);

        // GuestPhysical is disjoint from process_private and adds to victory_commit.
        alloc(Kind::GuestPhysical, 16 << 20);
        let with_guest = commit_breakdown(1000 << 20, 2000 << 20);
        free(Kind::GuestPhysical, 16 << 20);
        assert_eq!(
            with_guest
                .victory_commit
                .saturating_sub(before.victory_commit),
            16 << 20
        );
    }

    /// A reservation shows as reserved, not committed; committing it moves
    /// it, and releasing it returns both to where they were.
    #[test]
    fn the_address_space_walk_tells_reserved_from_committed() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const SIZE: usize = 256 << 20;
        // SAFETY: a fresh reservation, released at the end.
        let p = unsafe { VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!p.is_null());
        let reserved = space();
        // SAFETY: committing our own reservation.
        assert!(!unsafe { VirtualAlloc(p, SIZE, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        let committed = space();
        // Other threads allocate meanwhile; a 256 MiB step stands out.
        let moved = committed.private as i128 - reserved.private as i128;
        assert!(moved > (SIZE as i128) * 3 / 4, "{moved}");
        // SAFETY: our reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        let released = space();
        let back = committed.private as i128 - released.private as i128;
        assert!(back > (SIZE as i128) * 3 / 4, "{back}");
        assert!(space_summary().contains("reserved"));
    }

    /// The guard relieves after 3 low checks in a row, stops only if still
    /// low after relief, and any good check starts over.
    #[test]
    fn the_commit_guard_relieves_before_it_stops() {
        assert_eq!(critical_floor(10 << 30), 1 << 30);
        assert_eq!(critical_floor(100 << 30), 2 << 30);
        assert_eq!(guard_step(0, false, false), Guard::Fine);
        assert_eq!(guard_step(0, true, false), Guard::Low(1));
        assert_eq!(guard_step(1, true, false), Guard::Low(2));
        assert_eq!(
            guard_step(2, true, false),
            Guard::Low(3),
            "relief comes first"
        );
        assert_eq!(
            guard_step(3, true, true),
            Guard::Low(4),
            "time for relief to show"
        );
        assert_eq!(guard_step(5, true, true), Guard::Stop);
        assert_eq!(guard_step(5, false, true), Guard::Fine);
        assert_eq!(
            guard_step(0, true, true),
            Guard::Low(1),
            "a short dip never stops"
        );
    }

    #[test]
    fn pressure_needs_less_headroom_than_the_floor() {
        assert_eq!(pressure_floor(10 << 30), 2 << 30);
        assert_eq!(pressure_floor(100 << 30), 5 << 30);
        assert!(
            under_pressure(u64::MAX / 2),
            "an impossible size is pressure"
        );
    }

    /// A tracked object's bytes are counted until the object is destroyed.
    #[test]
    fn tracked_objects_are_uncounted_when_destroyed() {
        use windows::Win32::Graphics::Direct3D12::ID3D12Object;
        use windows::core::Interface;
        let Some(host) = crate::host::host() else {
            return;
        };
        let (buffer, _) = host.upload_buffer(1 << 16).expect("buffer");
        let object: ID3D12Object = buffer.cast().expect("object");
        // A distinctive size tells this tracker apart from other tests'.
        let bytes = 12_345_679;
        let made_before = made(Kind::DescriptorHeap);
        track(&object, Kind::DescriptorHeap, bytes);
        assert!(made(Kind::DescriptorHeap) > made_before);
        let dropped = || DROPPED.lock().unwrap().contains(&bytes);
        assert!(!dropped(), "counted while the object lives");
        drop(object);
        assert!(!dropped(), "another reference keeps it alive");
        drop(buffer);
        assert!(dropped(), "uncounted when destroyed");
    }
}
