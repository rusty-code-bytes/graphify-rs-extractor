//! Bounded input admission and persistent workers. Input bytes are an admission
//! estimate, not an RSS bound: parsers and extracted facts can expand the input.
use crate::{Stats, profile, store::Store};
use anyhow::{Context, Result, anyhow};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
};

const FILES_PER_BATCH: usize = 32;

struct Batch {
    paths: Vec<(PathBuf, usize)>,
    input_bytes: usize,
}

struct Batches<I> {
    entries: I,
    pending: Option<(PathBuf, usize)>,
    workers: usize,
    input_bytes: usize,
}

impl<I: Iterator<Item = Result<(PathBuf, usize)>>> Batches<I> {
    fn next_wave(&mut self) -> Result<Vec<Batch>> {
        let mut batches = Vec::new();
        let mut admitted = 0usize;
        let mut files = 0usize;
        let target = self.input_bytes.div_ceil(self.workers);
        for _ in 0..self.workers {
            let mut batch = Batch {
                paths: Vec::new(),
                input_bytes: 0,
            };
            while batch.paths.len() < FILES_PER_BATCH {
                let Some((path, bytes)) = self
                    .pending
                    .take()
                    .map(Ok)
                    .or_else(|| self.entries.next())
                    .transpose()?
                else {
                    break;
                };
                // A file larger than the admission budget must still be parsed
                // (or explicitly rejected by extraction); admit it alone.
                if files > 0 && bytes > self.input_bytes.saturating_sub(admitted) {
                    self.pending = Some((path, bytes));
                    break;
                }
                batch.paths.push((path, bytes));
                files += 1;
                batch.input_bytes = batch.input_bytes.saturating_add(bytes);
                admitted = admitted.saturating_add(bytes);
                if batch.input_bytes >= target {
                    break;
                }
            }
            if batch.paths.is_empty() {
                break;
            }
            batches.push(batch);
            if admitted >= self.input_bytes {
                break;
            }
        }
        Ok(batches)
    }
}

/// Reuse workers and their disk-backed stores across input-ordered waves. A
/// worker receives its next batch only after the preceding wave is fully merged,
/// so its shard can be cleared without racing the merge reader.
pub(crate) fn extract<I, P, M>(
    entries: impl Iterator<Item = Result<(PathBuf, usize)>>,
    workers: usize,
    input_bytes: usize,
    workspace: &Path,
    initialize: I,
    mut merge: M,
) -> Result<()>
where
    I: Fn() -> Result<P> + Sync,
    P: FnMut(&Path, usize, &Store, &mut Stats) -> Result<()>,
    M: FnMut(&Path, Stats) -> Result<()>,
{
    let mut batches = Batches {
        entries,
        pending: None,
        workers,
        input_bytes,
    };
    // Avoid opening stores or starting threads for an empty input.
    let first = batches.next_wave()?;
    if first.is_empty() {
        return Ok(());
    }
    std::thread::scope(|scope| -> Result<()> {
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        let mut handles = Vec::new();
        let outcome = (|| -> Result<()> {
            let mut wave = first;
            loop {
                let count = wave.len();
                // Grow the pool only when admission permits more concurrent
                // batches; a large first file must not serialize later waves.
                while senders.len() < count {
                    let (sender, receiver, handle) = spawn_worker(scope, workspace, &initialize)?;
                    senders.push(sender);
                    receivers.push(receiver);
                    handles.push(handle);
                }
                let parse_span = profile::span(|| "parse_window".into());
                for (sender, batch) in senders.iter().zip(wave) {
                    sender
                        .send(batch)
                        .map_err(|_| anyhow!("parser worker disconnected"))?;
                }
                // Receive every result before propagating an error. Merging in
                // input order preserves first-wins provenance and reference rows.
                let results: Vec<_> = receivers
                    .iter()
                    .take(count)
                    .map(|receiver| {
                        receiver
                            .recv()
                            .map_err(|_| anyhow!("parser worker panicked or disconnected"))?
                    })
                    .collect();
                let results = results.into_iter().collect::<Result<Vec<_>>>()?;
                drop(parse_span);
                let merge_span = profile::span(|| "merge_window".into());
                for (path, stats) in results {
                    merge(&path, stats)?;
                }
                drop(merge_span);
                wave = batches.next_wave()?;
                if wave.is_empty() {
                    break;
                }
            }
            Ok(())
        })();
        drop(senders);
        let joins: Vec<_> = handles.into_iter().map(|handle| handle.join()).collect();
        outcome?;
        for joined in joins {
            joined.map_err(|_| anyhow!("parser worker panicked"))?;
        }
        Ok(())
    })
    .context("parallel extraction")
}

// Both channels are bounded; a worker owns at most one admitted batch and one
// shard, and waits for a new job before clearing its previous result.
type WorkerResult = Result<(PathBuf, Stats)>;
type Worker<'scope> = (
    mpsc::SyncSender<Batch>,
    mpsc::Receiver<WorkerResult>,
    std::thread::ScopedJoinHandle<'scope, ()>,
);

fn spawn_worker<'scope, I, P>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    workspace: &'scope Path,
    initialize: &'scope I,
) -> Result<Worker<'scope>>
where
    I: Fn() -> Result<P> + Sync,
    P: FnMut(&Path, usize, &Store, &mut Stats) -> Result<()>,
{
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Batch>(0);
    let (results_tx, results_rx) = mpsc::sync_channel(1);
    let handle = std::thread::Builder::new().spawn_scoped(scope, move || {
        let mut local = None;
        while let Ok(batch) = jobs_rx.recv() {
            let result = (|| -> Result<_> {
                if local.is_none() {
                    let shard = tempfile::tempdir_in(workspace)?;
                    let store = Store::open(&shard.path().join("facts.sqlite"))?;
                    local = Some((store, shard, initialize()?));
                }
                let (store, shard, process) = local.as_mut().unwrap();
                store.clear_facts()?;
                store.begin()?;
                let mut stats = Stats::default();
                for (path, bytes) in batch.paths {
                    process(&path, bytes, store, &mut stats)?;
                }
                store.commit()?;
                Ok((shard.path().join("facts.sqlite"), stats))
            })();
            let failed = result.is_err();
            if results_tx.send(result).is_err() || failed {
                break;
            }
        }
    })?;
    Ok((jobs_tx, results_rx, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn skewed_batches_preserve_order_and_bound_admitted_input() {
        let weights = [0, 0, 18, 1, 1, 7, 2, 18, 0, 1, 2, 3, 4, 5, 6];
        let entries = weights
            .into_iter()
            .enumerate()
            .map(|(i, size)| Ok((PathBuf::from(i.to_string()), size)));
        let mut batches = Batches {
            entries,
            pending: None,
            workers: 3,
            input_bytes: 10,
        };
        let mut seen = Vec::new();
        loop {
            let wave = batches.next_wave().unwrap();
            if wave.is_empty() {
                break;
            }
            let bytes = wave.iter().map(|batch| batch.input_bytes).sum::<usize>();
            let files = wave.iter().map(|batch| batch.paths.len()).sum::<usize>();
            assert!(bytes <= 10 || files == 1);
            assert!(wave.len() <= 3);
            for batch in wave {
                seen.extend(batch.paths.into_iter().map(|(path, _)| path));
            }
        }
        assert_eq!(
            seen,
            (0..weights.len())
                .map(|i| PathBuf::from(i.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_files_are_bounded_by_batch_count() {
        let entries = (0..1000).map(|i| Ok((PathBuf::from(i.to_string()), 0)));
        let mut batches = Batches {
            entries,
            pending: None,
            workers: 2,
            input_bytes: 10,
        };
        let wave = batches.next_wave().unwrap();
        assert_eq!(wave.len(), 2);
        assert!(
            wave.iter()
                .all(|batch| batch.paths.len() == FILES_PER_BATCH)
        );
    }

    #[test]
    fn persistent_pool_grows_after_large_file_and_shards_hold_only_current_batch() {
        let workspace = tempfile::tempdir().unwrap();
        let initializations = AtomicUsize::new(0);
        let entries =
            (0..13).map(|i| Ok((PathBuf::from(i.to_string()), if i == 0 { 11 } else { 1 })));
        let mut shards = HashSet::new();
        let mut files = 0;
        extract(
            entries,
            2,
            4,
            workspace.path(),
            || {
                initializations.fetch_add(1, Ordering::SeqCst);
                Ok(|path: &Path, _: usize, store: &Store, stats: &mut Stats| {
                    let file = Node::file(path.to_str().unwrap(), "php");
                    store.node(&file)?;
                    store.reference(
                        &file.id,
                        "php-function",
                        "missing",
                        None,
                        "calls",
                        &file.label,
                        1,
                    )?;
                    stats.files += 1;
                    Ok(())
                })
            },
            |path, stats| {
                shards.insert(path.to_owned());
                let conn = rusqlite::Connection::open(path)?;
                let nodes: u64 =
                    conn.query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))?;
                let refs: u64 =
                    conn.query_row("SELECT count(*) FROM refs", [], |row| row.get(0))?;
                assert_eq!(nodes, stats.files, "prior batch leaked nodes");
                assert_eq!(refs, stats.files, "prior batch leaked references");
                files += stats.files;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(files, 13);
        assert_eq!(initializations.load(Ordering::SeqCst), 2);
        assert_eq!(shards.len(), 2);
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
    }

    #[test]
    fn worker_panic_returns_error_and_removes_shards() {
        let workspace = tempfile::tempdir().unwrap();
        let entries = [Ok((PathBuf::from("panic.php"), 1))].into_iter();
        let result = extract(
            entries,
            2,
            4,
            workspace.path(),
            || {
                Ok(
                    |_: &Path, _: usize, _: &Store, _: &mut Stats| -> Result<()> {
                        panic!("test parser panic")
                    },
                )
            },
            |_, _| Ok(()),
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("panicked"));
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
    }
}
