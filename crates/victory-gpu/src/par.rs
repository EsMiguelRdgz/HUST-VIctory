//! Bounded CPU parallelism for independent host-side work (copies of title
//! memory into distinct host buffers). Work runs on the calling thread when
//! it is small, so short submissions pay nothing for threads; past that it
//! is shared by at most [`workers`] scoped threads, the caller among them,
//! each taking the next item. Nothing is queued or retained between calls,
//! so memory use does not grow with the work.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Threads that may share a call's work, the caller included: from
/// `VICTORY_COPY_WORKERS` if set (1 runs everything on the caller), else
/// one less than the host's hardware threads, at most 4.
pub(crate) fn workers() -> usize {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        let set = std::env::var("VICTORY_COPY_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        set.unwrap_or(threads.saturating_sub(1).min(4)).clamp(1, 16)
    })
}

/// Calls `f` once for each item, sharing them among up to `workers` threads
/// when there are at least two and their `weight`s add up to at least
/// `threshold`; otherwise on the calling thread, in order. Heavier items
/// should come first for an even share.
pub(crate) fn for_each<T: Sync>(
    items: &[T],
    workers: usize,
    threshold: usize,
    weight: impl Fn(&T) -> usize,
    f: impl Fn(&T) + Sync,
) {
    let total: usize = items.iter().map(&weight).sum();
    let threads = workers.min(items.len());
    if threads < 2 || total < threshold {
        items.iter().for_each(f);
        return;
    }
    let next = AtomicUsize::new(0);
    let work = || {
        while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
            f(item);
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(work);
        }
        work();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn small_work_stays_on_the_caller_in_order() {
        let seen = Mutex::new(Vec::new());
        let me = std::thread::current().id();
        for_each(
            &[1, 2, 3],
            4,
            100,
            |&n| n,
            |&n| {
                assert_eq!(std::thread::current().id(), me);
                seen.lock().unwrap().push(n);
            },
        );
        assert_eq!(*seen.lock().unwrap(), [1, 2, 3]);
    }

    #[test]
    fn big_work_runs_each_item_once_on_bounded_threads() {
        let items: Vec<usize> = (0..64).collect();
        let done: Vec<AtomicUsize> = (0..64).map(|_| AtomicUsize::new(0)).collect();
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let threads = Mutex::new(std::collections::HashSet::new());
        for_each(
            &items,
            3,
            1,
            |_| 1,
            |&i| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                threads.lock().unwrap().insert(std::thread::current().id());
                std::thread::sleep(std::time::Duration::from_millis(1));
                done[i].fetch_add(1, Ordering::SeqCst);
                running.fetch_sub(1, Ordering::SeqCst);
            },
        );
        assert!(done.iter().all(|d| d.load(Ordering::SeqCst) == 1));
        assert!(most.load(Ordering::SeqCst) <= 3, "at most 3 at once");
        assert!(threads.lock().unwrap().len() <= 3);
    }

    #[test]
    fn worker_count_is_bounded() {
        assert!((1..=16).contains(&workers()));
    }
}
