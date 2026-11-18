//! Background queue for uploads and downloads.
//!
//! Transfers run on a small pool of worker threads. Progress is batched and
//! pushed to the UI a few times per second instead of once per chunk.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::s3::{Error, S3Client};

const WORKERS: usize = 4;
const NOTIFY_INTERVAL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Active,
    Done,
    Failed,
    Cancelled,
}

impl Status {
    fn is_finished(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Cancelled)
    }
}

/// What the caller provides to queue a transfer.
pub struct Request {
    pub kind: Kind,
    pub client: Arc<S3Client>,
    pub conn_id: String,
    pub bucket: String,
    pub key: String,
    pub path: PathBuf,
    pub size: u64,
}

struct Transfer {
    id: u64,
    request: Request,
    done: AtomicU64,
    state: Mutex<(Status, String)>,
    cancel: AtomicBool,
    dirty: AtomicBool,
}

/// Snapshot of a transfer as shown in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferView {
    pub id: u64,
    pub kind: Kind,
    pub conn_id: String,
    pub bucket: String,
    pub key: String,
    pub path: String,
    pub size: u64,
    pub done: u64,
    pub status: Status,
    pub error: String,
}

impl Transfer {
    fn view(&self) -> TransferView {
        let (status, error) = self.state.lock().unwrap().clone();
        TransferView {
            id: self.id,
            kind: self.request.kind,
            conn_id: self.request.conn_id.clone(),
            bucket: self.request.bucket.clone(),
            key: self.request.key.clone(),
            path: self.request.path.to_string_lossy().into_owned(),
            size: self.request.size,
            done: self.done.load(Ordering::Relaxed),
            status,
            error,
        }
    }

    fn set_status(&self, status: Status, error: String) {
        *self.state.lock().unwrap() = (status, error);
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn status(&self) -> Status {
        self.state.lock().unwrap().0
    }

    fn run(&self) {
        // Cancelled while still waiting in the queue.
        if self.cancel.load(Ordering::Relaxed) {
            self.set_status(Status::Cancelled, String::new());
            return;
        }
        self.set_status(Status::Active, String::new());

        let progress = |done: u64| {
            self.done.store(done, Ordering::Relaxed);
            self.dirty.store(true, Ordering::Relaxed);
        };
        let request = &self.request;
        let result = match request.kind {
            Kind::Upload => request.client.upload_file(
                &request.bucket,
                &request.key,
                &request.path,
                &progress,
                &self.cancel,
            ),
            Kind::Download => request.client.download_file(
                &request.bucket,
                &request.key,
                &request.path,
                &progress,
                &self.cancel,
            ),
        };
        match result {
            Ok(()) => self.set_status(Status::Done, String::new()),
            Err(Error::Cancelled) => self.set_status(Status::Cancelled, String::new()),
            Err(_) if self.cancel.load(Ordering::Relaxed) => {
                self.set_status(Status::Cancelled, String::new())
            }
            Err(err) => self.set_status(Status::Failed, err.to_string()),
        }
    }
}

struct Inner {
    queue: Mutex<VecDeque<Arc<Transfer>>>,
    wake: Condvar,
    all: Mutex<Vec<Arc<Transfer>>>,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub struct Transfers {
    inner: Arc<Inner>,
}

impl Transfers {
    /// Starts the worker pool. `notify` receives the transfers that changed
    /// since the previous call.
    pub fn start(notify: impl Fn(Vec<TransferView>) + Send + 'static) -> Self {
        let inner = Arc::new(Inner {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            all: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        });

        for _ in 0..WORKERS {
            let inner = inner.clone();
            thread::spawn(move || loop {
                let transfer = {
                    let mut queue = inner.queue.lock().unwrap();
                    loop {
                        match queue.pop_front() {
                            Some(transfer) => break transfer,
                            None => queue = inner.wake.wait(queue).unwrap(),
                        }
                    }
                };
                transfer.run();
            });
        }

        let notifier = inner.clone();
        thread::spawn(move || loop {
            thread::sleep(NOTIFY_INTERVAL);
            let changed: Vec<TransferView> = notifier
                .all
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.dirty.swap(false, Ordering::Relaxed))
                .map(|t| t.view())
                .collect();
            if !changed.is_empty() {
                notify(changed);
            }
        });

        Self { inner }
    }

    pub fn enqueue(&self, requests: Vec<Request>) -> usize {
        let count = requests.len();
        let transfers: Vec<Arc<Transfer>> = requests
            .into_iter()
            .map(|request| {
                Arc::new(Transfer {
                    id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
                    request,
                    done: AtomicU64::new(0),
                    state: Mutex::new((Status::Queued, String::new())),
                    cancel: AtomicBool::new(false),
                    dirty: AtomicBool::new(true),
                })
            })
            .collect();

        self.inner
            .all
            .lock()
            .unwrap()
            .extend(transfers.iter().cloned());
        self.inner.queue.lock().unwrap().extend(transfers);
        self.inner.wake.notify_all();
        count
    }

    pub fn list(&self) -> Vec<TransferView> {
        self.inner
            .all
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.view())
            .collect()
    }

    /// Cancels one transfer, or every unfinished one when `id` is `None`.
    pub fn cancel(&self, id: Option<u64>) {
        for transfer in self.inner.all.lock().unwrap().iter() {
            if id.is_some_and(|id| id != transfer.id) || transfer.status().is_finished() {
                continue;
            }
            transfer.cancel.store(true, Ordering::Relaxed);
            // Queued transfers are marked right away; active ones stop at
            // their next chunk and report back themselves.
            if transfer.status() == Status::Queued {
                transfer.set_status(Status::Cancelled, String::new());
            }
        }
    }

    /// Puts a failed or cancelled transfer back in the queue.
    pub fn retry(&self, id: u64) {
        let all = self.inner.all.lock().unwrap();
        let Some(transfer) = all.iter().find(|t| t.id == id) else {
            return;
        };
        if !matches!(transfer.status(), Status::Failed | Status::Cancelled) {
            return;
        }
        transfer.cancel.store(false, Ordering::Relaxed);
        transfer.done.store(0, Ordering::Relaxed);
        transfer.set_status(Status::Queued, String::new());
        self.inner.queue.lock().unwrap().push_back(transfer.clone());
        self.inner.wake.notify_one();
    }

    /// Forgets finished transfers.
    pub fn clear_finished(&self) {
        self.inner
            .all
            .lock()
            .unwrap()
            .retain(|t| !t.status().is_finished());
    }

    /// True while anything is queued or running.
    pub fn is_busy(&self) -> bool {
        self.inner
            .all
            .lock()
            .unwrap()
            .iter()
            .any(|t| !t.status().is_finished())
    }
}
