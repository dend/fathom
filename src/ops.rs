//! Bulk object operations: delete, copy, move and rename.
//!
//! S3 has no folders and no rename, so these fan out into one request per
//! object, run a few at a time.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

use serde::Deserialize;

use crate::s3::{Error, Result, S3Client};

const PARALLELISM: usize = 8;

/// Reports `(completed, total)` object operations.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);

/// One entry to copy or move. For folders, `from` and `to` are prefixes
/// ending in `/`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Move {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub folder: bool,
    #[serde(default)]
    pub size: u64,
}

/// Runs `work` over `items` on a few threads, stopping at the first error.
fn for_each<T: Sync>(
    items: &[T],
    cancel: &AtomicBool,
    on_item_done: &(dyn Fn() + Sync),
    work: &(dyn Fn(&T) -> Result<()> + Sync),
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        for _ in 0..PARALLELISM.min(items.len()) {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if cancel.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Relaxed);
                    failure.lock().unwrap().get_or_insert(Error::Cancelled);
                    return;
                }
                let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    return;
                };
                match work(item) {
                    Ok(()) => on_item_done(),
                    Err(err) => {
                        stop.store(true, Ordering::Relaxed);
                        failure.lock().unwrap().get_or_insert(err);
                        return;
                    }
                }
            });
        }
    });

    match failure.into_inner().unwrap() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Deletes the given objects and everything under the given prefixes.
/// Returns the number of objects removed.
pub fn delete(
    client: &S3Client,
    bucket: &str,
    keys: &[String],
    prefixes: &[String],
    cancel: &AtomicBool,
    progress: Progress<'_>,
) -> Result<usize> {
    let mut targets: Vec<String> = keys.to_vec();
    for prefix in prefixes {
        if prefix.is_empty() {
            return Err(Error::Invalid("Refusing to delete an entire bucket".into()));
        }
        targets.extend(
            client
                .list_all(bucket, prefix, cancel)?
                .into_iter()
                .map(|o| o.key),
        );
    }
    targets.sort();
    targets.dedup();

    let total = targets.len();
    let done = AtomicUsize::new(0);
    progress(0, total);
    for_each(
        &targets,
        cancel,
        &|| progress(done.fetch_add(1, Ordering::Relaxed) + 1, total),
        &|key| client.delete(bucket, key),
    )?;
    Ok(total)
}

/// Copies objects server-side and, when `delete_source` is set, removes the
/// originals afterwards (a move or rename). Returns the number of objects copied.
pub fn transfer(
    client: &S3Client,
    src_bucket: &str,
    dst_bucket: &str,
    moves: &[Move],
    delete_source: bool,
    cancel: &AtomicBool,
    progress: Progress<'_>,
) -> Result<usize> {
    // (source key, destination key, size)
    let mut plan: Vec<(String, String, u64)> = Vec::new();
    for item in moves {
        if item.from.is_empty() || item.to.is_empty() {
            return Err(Error::Invalid(
                "Source and destination must not be empty".into(),
            ));
        }
        if src_bucket == dst_bucket && item.from == item.to {
            return Err(Error::Invalid("Source and destination are the same".into()));
        }
        if !item.folder {
            plan.push((item.from.clone(), item.to.clone(), item.size));
            continue;
        }
        if !item.from.ends_with('/') || !item.to.ends_with('/') {
            return Err(Error::Invalid("Folder paths must end with a slash".into()));
        }
        if src_bucket == dst_bucket && item.to.starts_with(&item.from) {
            return Err(Error::Invalid(
                "A folder cannot be placed inside itself".into(),
            ));
        }
        for object in client.list_all(src_bucket, &item.from, cancel)? {
            let relative = &object.key[item.from.len()..];
            plan.push((
                object.key.clone(),
                format!("{}{relative}", item.to),
                object.size,
            ));
        }
    }

    let copies = plan.len();
    let total = if delete_source { copies * 2 } else { copies };
    let done = AtomicUsize::new(0);
    let tick = || progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
    progress(0, total);

    for_each(&plan, cancel, &tick, &|(from, to, size)| {
        client.copy(src_bucket, from, dst_bucket, to, *size, cancel)
    })?;
    // Sources are only removed once every copy has succeeded, so a failure
    // midway never loses data.
    if delete_source {
        for_each(&plan, cancel, &tick, &|(from, _, _)| {
            client.delete(src_bucket, from)
        })?;
    }
    Ok(copies)
}
