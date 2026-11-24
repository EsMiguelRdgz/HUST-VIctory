//! Where host memory goes: live bytes of the allocations this crate makes
//! for copies of title memory, with the process's private commit, the
//! system's commit and the host GPU's memory budgets, logged with the
//! frame-rate line so that growth in an authorized run can be attributed.
//! Counting is a few relaxed atomic adds per allocation and free.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What a counted allocation is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Host copies of title buffer objects.
    Buffer,
    /// Host copies of aligned windows of title memory outside buffers.
    Loose,
    /// Dedicated copies for buffer views whose offset needs realigning.
    Aligned,
    /// Pre-GPU snapshots held by submissions in flight.
    Snapshot,
    /// Snapshot buffers kept for reuse.
    SnapshotPool,
}

const KINDS: usize = 5;
const NAMES: [&str; KINDS] = ["buffers", "loose", "aligned", "snapshots", "snapshot pool"];

/// Live bytes, live allocations, peak live bytes.
struct Counter([AtomicU64; 3]);

impl Counter {
    const fn new() -> Counter {
        Counter([const { AtomicU64::new(0) }; 3])
    }

    fn alloc(&self, bytes: u64) {
        let [live, count, peak] = &self.0;
        let now = live.fetch_add(bytes, Relaxed) + bytes;
        count.fetch_add(1, Relaxed);
        peak.fetch_max(now, Relaxed);
    }

    fn free(&self, bytes: u64) {
        let [live, count, _] = &self.0;
        // Saturating: never wrap on a miscount, which would hide real growth.
        let _ = live.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(bytes)));
        let _ = count.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)));
    }

    fn get(&self) -> (u64, u64, u64) {
        let [live, count, peak] = &self.0;
        (live.load(Relaxed), count.load(Relaxed), peak.load(Relaxed))
    }
}

static LIVE: [Counter; KINDS] = [const { Counter::new() }; KINDS];

/// `bytes` more of `kind` are live.
pub(crate) fn alloc(kind: Kind, bytes: u64) {
    LIVE[kind as usize].alloc(bytes);
}

/// `bytes` of `kind` were freed (as counted by [`alloc`]).
pub(crate) fn free(kind: Kind, bytes: u64) {
    LIVE[kind as usize].free(bytes);
}

/// Live bytes, live allocations and peak bytes of `kind`.
pub(crate) fn live(kind: Kind) -> (u64, u64, u64) {
    LIVE[kind as usize].get()
}

/// The process's private commit and working set, in bytes.
pub(crate) fn process() -> Option<(u64, u64)> {
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
pub(crate) fn system_commit() -> Option<(u64, u64)> {
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

/// The host GPU's memory use and budget, local (video memory) then
/// non-local (system memory the GPU uses), in bytes.
pub(crate) type Video = [(u64, u64); 2];

/// One line: the counted allocations, then process, system and GPU memory.
pub(crate) fn summary(video: Option<Video>) -> String {
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    let mut text = String::from("host memory:");
    if let Some((private, working)) = process() {
        text.push_str(&format!(
            " private {:.0} MB, working set {:.0} MB;",
            mb(private),
            mb(working)
        ));
    }
    if let Some((total, limit)) = system_commit() {
        text.push_str(&format!(
            " system commit {:.0}/{:.0} MB;",
            mb(total),
            mb(limit)
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
    text.push_str(" copies of title memory:");
    for (i, name) in NAMES.iter().enumerate() {
        let (bytes, count, peak) = live(KINDS_ALL[i]);
        text.push_str(&format!(
            " {name} {count} = {:.1} MB (peak {:.1}),",
            mb(bytes),
            mb(peak)
        ));
    }
    text.pop();
    text
}

const KINDS_ALL: [Kind; KINDS] = [
    Kind::Buffer,
    Kind::Loose,
    Kind::Aligned,
    Kind::Snapshot,
    Kind::SnapshotPool,
];

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
    }

    #[test]
    fn process_and_system_memory_are_reported() {
        let (private, working) = process().expect("process counters");
        assert!(private > 0 && working > 0);
        let (total, limit) = system_commit().expect("performance information");
        assert!(total > 0 && limit >= total);
        let line = summary(Some([(1 << 20, 2 << 20), (0, 4 << 20)]));
        assert!(line.contains("private") && line.contains("GPU local 1/2 MB"));
        assert!(line.contains("snapshot pool"));
    }
}
