//! The native UI, drawn on the GPU with egui.
//!
//! Everything that touches the network or the disk runs on worker threads and
//! reports back through [`Event`]s, so the UI thread never blocks on I/O.

mod actions;
mod dialogs;
mod icons;
mod list;
mod panels;
mod preview;
mod theme;
mod tree;
mod updates;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, PointerButton};

use crate::config::{Connection, Store};
use crate::format;
use crate::proxy::PreviewServer;
use crate::s3::{Bucket, S3Client};
use crate::system;
use crate::transfers::{Status, TransferView, Transfers};

/// Listings are loaded page by page up to this many entries.
const MAX_ITEMS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Bucket,
    Folder,
    File,
}

/// One row of the file list.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    /// Object key, folder prefix (ending in `/`) or bucket name. Unique in a listing.
    pub key: String,
    pub name: String,
    pub size: u64,
    pub modified: Option<i64>,
    pub etag: String,
}

impl Item {
    fn is_file(&self) -> bool {
        self.kind == ItemKind::File
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    pub bucket: Option<String>,
    pub prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Size,
    Type,
    Modified,
}

#[derive(Default)]
struct Listing {
    items: Vec<Item>,
    loading: bool,
    error: Option<String>,
    generation: u64,
    capped: bool,
    /// A soft reload keeps the current items on screen until the new ones are complete.
    soft: bool,
    incoming: Vec<Item>,
    folders: Vec<String>,
}

#[derive(Default)]
struct TreeNode {
    children: Option<Vec<String>>,
    expanded: bool,
    loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipMode {
    Cut,
    Copy,
}

struct Clip {
    mode: ClipMode,
    conn: String,
    bucket: String,
    items: Vec<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Error,
}

struct Toast {
    id: u64,
    text: String,
    kind: ToastKind,
    until: Instant,
}

/// A bulk delete, copy or move in progress.
struct RunningOp {
    id: u64,
    label: String,
    done: usize,
    total: usize,
    cancel: Arc<AtomicBool>,
}

pub enum Event {
    Buckets {
        conn: String,
        result: Result<Vec<Bucket>, String>,
    },
    Domain {
        conn: String,
        bucket: String,
        url: Option<String>,
    },
    Listing {
        generation: u64,
        items: Vec<Item>,
        folders: Vec<String>,
        done: bool,
        capped: bool,
    },
    ListingFailed {
        generation: u64,
        error: String,
    },
    TreeChildren {
        conn: String,
        bucket: String,
        prefix: String,
        result: Result<Vec<String>, String>,
    },
    Preview(preview::Loaded),
    Transfers(Vec<TransferView>),
    OpProgress {
        id: u64,
        done: usize,
        total: usize,
    },
    OpFinished {
        id: u64,
        result: Result<String, String>,
        /// Buckets (and optionally one prefix in them) whose listings changed.
        changed: Vec<(String, Option<String>)>,
        select: Option<String>,
    },
    Dialog(dialogs::Reply),
    Toast {
        text: String,
        error: bool,
    },
    Queued {
        count: usize,
        verb: &'static str,
    },
    Update(updates::Reply),
}

pub struct FathomApp {
    store: Arc<Store>,
    transfers: Transfers,
    proxy: Arc<PreviewServer>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,

    connections: Vec<Connection>,
    active: Option<String>,
    buckets: Vec<Bucket>,
    buckets_loading: bool,
    buckets_error: Option<String>,
    /// Public base URLs looked up with a connection's API token, by connection
    /// and bucket.
    discovered: HashMap<(String, String), String>,

    location: Location,
    history: Vec<Location>,
    history_index: usize,
    listing: Listing,
    /// Indices into `listing.items`, filtered and sorted.
    view: Vec<usize>,
    view_dirty: bool,
    view_built: Instant,
    sort: (Column, bool),
    filter: String,

    selection: HashSet<String>,
    cursor: Option<String>,
    anchor: Option<String>,
    pending_select: Option<String>,
    reveal: Option<String>,
    clipboard: Option<Clip>,

    tree: HashMap<(String, String), TreeNode>,
    preview: preview::Pane,
    viewer: Option<preview::Viewer>,

    transfer_views: BTreeMap<u64, TransferView>,
    batch_start: u64,
    show_transfers: bool,
    show_preview: bool,

    ops: Vec<RunningOp>,
    next_id: u64,
    toasts: Vec<Toast>,
    modal: Option<dialogs::Modal>,
    /// The graphics API and adapter, for the About dialog.
    graphics: String,
    updates: updates::Updates,

    refresh_at: Option<Instant>,
    tree_refresh_at: Option<(Instant, HashSet<String>)>,
    typed: String,
    typed_at: Instant,
    list_scroll: list::Scroll,
    /// Where the file list was drawn last frame: the drop zone for dragged files.
    list_rect: Option<egui::Rect>,
}

/// UI preferences remembered between runs (window size and panel widths are
/// remembered by eframe itself).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Prefs {
    show_preview: bool,
    show_transfers: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            show_preview: true,
            show_transfers: false,
        }
    }
}

impl FathomApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install(&ctx);
        let prefs: Prefs = cc
            .storage
            .and_then(|storage| storage.get_string("prefs"))
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();

        let (tx, rx) = channel();
        let transfers = {
            let tx = tx.clone();
            let ctx = ctx.clone();
            Transfers::start(move |changed| {
                let _ = tx.send(Event::Transfers(changed));
                ctx.request_repaint();
            })
        };
        let proxy =
            Arc::new(PreviewServer::start().expect("could not start the local preview server"));

        let settings = system::config_dir();
        let mut app = Self {
            updates: updates::Updates::new(&settings),
            store: Arc::new(Store::load(settings)),
            transfers,
            proxy,
            tx,
            rx,
            ctx,
            connections: Vec::new(),
            active: None,
            buckets: Vec::new(),
            buckets_loading: false,
            buckets_error: None,
            discovered: HashMap::new(),
            location: Location::default(),
            history: Vec::new(),
            history_index: 0,
            listing: Listing::default(),
            view: Vec::new(),
            view_dirty: true,
            view_built: Instant::now(),
            sort: (Column::Name, false),
            filter: String::new(),
            selection: HashSet::new(),
            cursor: None,
            anchor: None,
            pending_select: None,
            reveal: None,
            clipboard: None,
            tree: HashMap::new(),
            preview: preview::Pane::default(),
            viewer: None,
            transfer_views: BTreeMap::new(),
            batch_start: 0,
            show_transfers: prefs.show_transfers,
            show_preview: prefs.show_preview,
            ops: Vec::new(),
            next_id: 1,
            toasts: Vec::new(),
            modal: None,
            graphics: cc
                .wgpu_render_state
                .as_ref()
                .map(|render| dialogs::graphics_name(&render.adapter.get_info()))
                .unwrap_or_default(),
            refresh_at: None,
            tree_refresh_at: None,
            typed: String::new(),
            typed_at: Instant::now(),
            list_scroll: list::Scroll::default(),
            list_rect: None,
        };
        app.load_connections();
        match app.active.clone() {
            Some(id) => app.switch_connection(&id),
            None => app.modal = Some(dialogs::Modal::connection(None)),
        }
        app
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Runs `job` on a worker thread and delivers its event to the UI.
    fn spawn(&self, job: impl FnOnce() -> Event + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    fn client(&self) -> Result<Arc<S3Client>, String> {
        let id = self.active.as_deref().ok_or("No connection")?;
        self.store.client(id)
    }

    fn active_connection(&self) -> Option<&Connection> {
        let id = self.active.as_deref()?;
        self.connections.iter().find(|c| c.id == id)
    }

    /// Public base URL of a bucket: one set by hand, or else one looked up with
    /// the connection's API token.
    fn domain_for(&self, bucket: &str) -> Option<String> {
        let connection = self.active_connection()?;
        connection.domains.get(bucket).cloned().or_else(|| {
            self.discovered
                .get(&(connection.id.clone(), bucket.to_string()))
                .cloned()
        })
    }

    fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        let id = self.next_id();
        let seconds = match kind {
            ToastKind::Error => 8,
            _ => 3,
        };
        self.toasts.push(Toast {
            id,
            text: text.into(),
            kind,
            until: Instant::now() + Duration::from_secs(seconds),
        });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    // ----- connections -----------------------------------------------------------

    fn load_connections(&mut self) {
        let config = self.store.snapshot();
        self.connections = config.connections;
        let known = self
            .connections
            .iter()
            .any(|c| Some(&c.id) == config.active.as_ref());
        self.active = if known {
            config.active
        } else {
            self.connections.first().map(|c| c.id.clone())
        };
    }

    fn switch_connection(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            let owned = id.to_string();
            let _ = self.store.update(|config| config.active = Some(owned));
        }
        self.active = Some(id.to_string());
        self.history.clear();
        self.history_index = 0;
        self.clipboard = None;
        self.tree.clear();
        self.buckets.clear();
        self.navigate(Location::default(), None);
        self.load_buckets();
    }

    fn load_buckets(&mut self) {
        let Some(conn) = self.active.clone() else {
            return;
        };
        self.buckets_loading = true;
        self.buckets_error = None;
        let store = self.store.clone();
        self.spawn(move || {
            let result = (|| {
                let connection = store.connection(&conn)?;
                if !connection.buckets.is_empty() {
                    return Ok(connection
                        .buckets
                        .into_iter()
                        .map(|name| Bucket {
                            name,
                            created: String::new(),
                        })
                        .collect());
                }
                store
                    .client(&conn)?
                    .list_buckets()
                    .map_err(|e| e.to_string())
            })();
            Event::Buckets { conn, result }
        });
        if self.location.bucket.is_none() {
            self.load_listing(true, None);
        }
    }

    /// Looks up each bucket's public domain through the Cloudflare API, for
    /// connections set up with an API token. Buckets with a domain set by hand
    /// are skipped.
    fn discover_domains(&mut self) {
        let Some(connection) = self.active_connection().cloned() else {
            return;
        };
        if !connection.uses_token {
            return;
        }
        let buckets: Vec<String> = self
            .buckets
            .iter()
            .map(|b| b.name.clone())
            .filter(|name| !connection.domains.contains_key(name))
            .collect();
        if buckets.is_empty() {
            return;
        }
        let store = self.store.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let Ok(Some(api)) = store.cloudflare(&connection.id) else {
                return;
            };
            let queue = Mutex::new(buckets.into_iter());
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| loop {
                        let Some(bucket) = queue.lock().unwrap().next() else {
                            break;
                        };
                        // A token that can't read bucket settings leaves the domain
                        // to be set by hand.
                        let url = api
                            .public_domain(&bucket, &connection.jurisdiction)
                            .ok()
                            .flatten();
                        let _ = tx.send(Event::Domain {
                            conn: connection.id.clone(),
                            bucket,
                            url,
                        });
                        ctx.request_repaint();
                    });
                }
            });
        });
    }

    // ----- navigation -------------------------------------------------------------

    fn navigate(&mut self, location: Location, select: Option<String>) {
        let location = Location {
            prefix: if location.bucket.is_some() {
                location.prefix
            } else {
                String::new()
            },
            bucket: location.bucket,
        };
        if self.history.get(self.history_index) != Some(&location) {
            self.history.truncate(self.history_index + 1);
            self.history.push(location.clone());
            self.history_index = self.history.len() - 1;
        }
        self.location = location;
        self.filter.clear();
        self.list_scroll.set = Some(0.0);
        self.reveal_location_in_tree();
        self.load_listing(false, select);
    }

    fn go_back(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.location = self.history[self.history_index].clone();
            self.filter.clear();
            self.reveal_location_in_tree();
            self.load_listing(false, None);
        }
    }

    fn go_forward(&mut self) {
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            self.location = self.history[self.history_index].clone();
            self.filter.clear();
            self.reveal_location_in_tree();
            self.load_listing(false, None);
        }
    }

    /// Goes to the containing folder and selects the folder that was just left.
    fn go_up(&mut self) {
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        if self.location.prefix.is_empty() {
            self.navigate(Location::default(), Some(bucket));
        } else {
            let prefix = self.location.prefix.clone();
            let parent = format::parent_prefix(&prefix).to_string();
            self.navigate(
                Location {
                    bucket: Some(bucket),
                    prefix: parent,
                },
                Some(prefix),
            );
        }
    }

    fn refresh(&mut self) {
        self.refresh_at = None;
        if self.location.bucket.is_some() {
            self.load_listing(true, None);
        } else {
            self.load_buckets();
        }
    }

    fn bucket_items(&self) -> Vec<Item> {
        self.buckets
            .iter()
            .map(|bucket| Item {
                kind: ItemKind::Bucket,
                key: bucket.name.clone(),
                name: bucket.name.clone(),
                size: 0,
                modified: format::parse_iso8601(&bucket.created),
                etag: String::new(),
            })
            .collect()
    }

    fn load_listing(&mut self, soft: bool, select: Option<String>) {
        self.listing.generation += 1;
        let generation = self.listing.generation;
        self.pending_select = select;
        self.listing.soft = soft;
        self.listing.error = None;
        self.listing.capped = false;
        self.listing.incoming.clear();
        self.listing.folders.clear();
        if !soft {
            self.listing.items.clear();
            self.view.clear();
            self.selection.clear();
            self.cursor = None;
            self.anchor = None;
        }
        self.view_dirty = true;

        let Some(bucket) = self.location.bucket.clone() else {
            self.listing.items = self.bucket_items();
            self.listing.loading = self.buckets_loading;
            self.listing.error = self.buckets_error.clone();
            self.finish_listing();
            return;
        };
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => {
                self.listing.loading = false;
                self.listing.error = Some(error);
                return;
            }
        };
        self.listing.loading = true;
        let prefix = self.location.prefix.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let mut token: Option<String> = None;
            let mut count = 0;
            loop {
                let event = match client.list(&bucket, &prefix, Some("/"), token.as_deref()) {
                    Ok(page) => {
                        let mut items =
                            Vec::with_capacity(page.prefixes.len() + page.objects.len());
                        for folder in &page.prefixes {
                            items.push(Item {
                                kind: ItemKind::Folder,
                                key: folder.clone(),
                                name: format::base_name(folder).to_string(),
                                size: 0,
                                modified: None,
                                etag: String::new(),
                            });
                        }
                        for object in page.objects {
                            // The zero-byte marker that makes an empty folder exist is not a file.
                            if object.key == prefix {
                                continue;
                            }
                            items.push(Item {
                                kind: ItemKind::File,
                                name: object
                                    .key
                                    .get(prefix.len()..)
                                    .unwrap_or(&object.key)
                                    .to_string(),
                                key: object.key,
                                size: object.size,
                                modified: format::parse_iso8601(&object.modified),
                                etag: object.etag,
                            });
                        }
                        count += items.len();
                        let capped = count >= MAX_ITEMS && page.next_token.is_some();
                        let done = page.next_token.is_none() || capped;
                        token = page.next_token;
                        Event::Listing {
                            generation,
                            items,
                            folders: page.prefixes,
                            done,
                            capped,
                        }
                    }
                    Err(error) => Event::ListingFailed {
                        generation,
                        error: error.to_string(),
                    },
                };
                let finished = !matches!(event, Event::Listing { done: false, .. });
                if tx.send(event).is_err() {
                    return;
                }
                ctx.request_repaint();
                if finished {
                    return;
                }
            }
        });
    }

    fn finish_listing(&mut self) {
        let present: HashSet<&str> = self
            .listing
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        if let Some(select) = self
            .pending_select
            .take()
            .filter(|key| present.contains(key.as_str()))
        {
            self.select_only(&select);
            self.reveal = Some(select);
        } else {
            self.selection.retain(|key| present.contains(key.as_str()));
            if self
                .cursor
                .as_deref()
                .is_some_and(|key| !present.contains(key))
            {
                self.cursor = None;
            }
        }
        self.view_dirty = true;
    }

    /// Rebuilds the filtered, sorted view of the listing.
    fn rebuild_view(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        let items = &self.listing.items;
        let mut view: Vec<usize> = (0..items.len())
            .filter(|&i| needle.is_empty() || items[i].name.to_lowercase().contains(&needle))
            .collect();
        let (column, descending) = self.sort;
        view.sort_by(|&a, &b| {
            let (a, b) = (&items[a], &items[b]);
            // Buckets and folders always come before files.
            let rank = |item: &Item| u8::from(item.is_file());
            rank(a).cmp(&rank(b)).then_with(|| {
                let order = match column {
                    Column::Name => std::cmp::Ordering::Equal,
                    Column::Size => a.size.cmp(&b.size),
                    Column::Type => format::compare_names(&type_of(a), &type_of(b)),
                    Column::Modified => a.modified.cmp(&b.modified),
                }
                .then_with(|| format::compare_names(&a.name, &b.name));
                if descending {
                    order.reverse()
                } else {
                    order
                }
            })
        });
        self.view = view;
        self.view_dirty = false;
        self.view_built = Instant::now();
    }

    // ----- selection ------------------------------------------------------------------

    fn select_only(&mut self, key: &str) {
        self.selection.clear();
        self.selection.insert(key.to_string());
        self.cursor = Some(key.to_string());
        self.anchor = Some(key.to_string());
    }

    fn selected_items(&self) -> Vec<Item> {
        self.view
            .iter()
            .map(|&i| &self.listing.items[i])
            .filter(|item| self.selection.contains(&item.key))
            .cloned()
            .collect()
    }

    fn view_position(&self, key: &str) -> Option<usize> {
        self.view
            .iter()
            .position(|&i| self.listing.items[i].key == key)
    }

    fn move_cursor(&mut self, delta: isize, extend: bool) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        let current = self
            .cursor
            .as_deref()
            .and_then(|key| self.view_position(key));
        let index = match (current, delta) {
            (_, isize::MIN) => 0,
            (_, isize::MAX) => last,
            (None, d) if d > 0 => 0,
            (None, _) => last,
            (Some(c), d) => (c as isize + d).clamp(0, last as isize) as usize,
        };
        let key = self.listing.items[self.view[index]].key.clone();
        match (
            extend,
            self.anchor.as_deref().and_then(|a| self.view_position(a)),
        ) {
            (true, Some(anchor)) => {
                let (from, to) = (anchor.min(index), anchor.max(index));
                self.selection = self.view[from..=to]
                    .iter()
                    .map(|&i| self.listing.items[i].key.clone())
                    .collect();
                self.cursor = Some(key.clone());
            }
            _ => self.select_only(&key),
        }
        self.reveal = Some(key);
    }

    fn type_ahead(&mut self, text: &str) {
        if self.typed_at.elapsed() > Duration::from_millis(700) {
            self.typed.clear();
        }
        self.typed.push_str(text);
        self.typed_at = Instant::now();
        let needle = self.typed.to_lowercase();
        let found = self
            .view
            .iter()
            .map(|&i| &self.listing.items[i])
            .find(|item| item.name.to_lowercase().starts_with(&needle))
            .map(|item| item.key.clone());
        if let Some(key) = found {
            self.select_only(&key);
            self.reveal = Some(key);
        }
    }

    // ----- tree --------------------------------------------------------------------------

    fn reveal_location_in_tree(&mut self) {
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        let mut ancestors = vec![String::new()];
        let parts: Vec<&str> = self
            .location
            .prefix
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        for i in 1..parts.len() {
            ancestors.push(format!("{}/", parts[..i].join("/")));
        }
        if self.location.prefix.is_empty() {
            ancestors.clear();
        }
        for ancestor in ancestors {
            let node = self
                .tree
                .entry((bucket.clone(), ancestor.clone()))
                .or_default();
            node.expanded = true;
            if node.children.is_none() && !node.loading {
                self.load_tree_children(&bucket, &ancestor);
            }
        }
    }

    fn load_tree_children(&mut self, bucket: &str, prefix: &str) {
        let Ok(client) = self.client() else {
            return;
        };
        let Some(conn) = self.active.clone() else {
            return;
        };
        self.tree
            .entry((bucket.to_string(), prefix.to_string()))
            .or_default()
            .loading = true;
        let (bucket, prefix) = (bucket.to_string(), prefix.to_string());
        self.spawn(move || {
            let result = (|| {
                let mut folders = Vec::new();
                let mut token = None;
                // Folder trees can be enormous; stop after a few pages.
                for _ in 0..10 {
                    let page = client
                        .list(&bucket, &prefix, Some("/"), token.as_deref())
                        .map_err(|e| e.to_string())?;
                    folders.extend(page.prefixes);
                    token = page.next_token;
                    if token.is_none() {
                        break;
                    }
                }
                Ok(folders)
            })();
            Event::TreeChildren {
                conn,
                bucket,
                prefix,
                result,
            }
        });
    }

    /// Something in `bucket` changed: reload what the tree shows of it.
    fn invalidate_tree(&mut self, bucket: &str, prefix: Option<&str>) {
        let stale: Vec<(String, String)> = self
            .tree
            .keys()
            .filter(|(b, p)| b == bucket && prefix.is_none_or(|prefix| p == prefix))
            .cloned()
            .collect();
        for key in stale {
            let expanded = self.tree.get(&key).is_some_and(|node| node.expanded);
            if expanded {
                self.load_tree_children(&key.0, &key.1);
            } else if let Some(node) = self.tree.get_mut(&key) {
                node.children = None;
            }
        }
    }

    // ----- events --------------------------------------------------------------------------

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.handle(event);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Buckets { conn, result } => {
                if self.active.as_deref() != Some(conn.as_str()) {
                    return;
                }
                self.buckets_loading = false;
                match result {
                    Ok(buckets) => {
                        self.buckets = buckets;
                        self.discover_domains();
                    }
                    Err(error) => {
                        self.buckets.clear();
                        self.buckets_error = Some(error);
                    }
                }
                if self.location.bucket.is_none() {
                    self.load_listing(true, None);
                }
            }
            Event::Domain { conn, bucket, url } => match url {
                Some(url) => {
                    self.discovered.insert((conn, bucket), url);
                }
                None => {
                    self.discovered.remove(&(conn, bucket));
                }
            },
            Event::Listing {
                generation,
                items,
                folders,
                done,
                capped,
            } => {
                if generation != self.listing.generation {
                    return;
                }
                self.listing.folders.extend(folders);
                if self.listing.soft {
                    self.listing.incoming.extend(items);
                } else {
                    self.listing.items.extend(items);
                }
                self.view_dirty = true;
                if done {
                    if self.listing.soft {
                        self.listing.items = std::mem::take(&mut self.listing.incoming);
                    }
                    self.listing.loading = false;
                    self.listing.capped = capped;
                    if !capped {
                        if let Some(bucket) = self.location.bucket.clone() {
                            let node = self
                                .tree
                                .entry((bucket, self.location.prefix.clone()))
                                .or_default();
                            node.children = Some(std::mem::take(&mut self.listing.folders));
                            node.loading = false;
                        }
                    }
                    self.finish_listing();
                }
            }
            Event::ListingFailed { generation, error } => {
                if generation != self.listing.generation {
                    return;
                }
                self.listing.loading = false;
                self.listing.error = Some(error);
                if !self.listing.soft {
                    self.listing.items.clear();
                }
                self.view_dirty = true;
            }
            Event::TreeChildren {
                conn,
                bucket,
                prefix,
                result,
            } => {
                if self.active.as_deref() != Some(conn.as_str()) {
                    return;
                }
                let node = self.tree.entry((bucket, prefix)).or_default();
                node.loading = false;
                match result {
                    Ok(children) => node.children = Some(children),
                    Err(_) => {
                        node.children = Some(Vec::new());
                        node.expanded = false;
                    }
                }
            }
            Event::Preview(loaded) => self.preview_loaded(loaded),
            Event::Transfers(changed) => self.transfers_changed(changed),
            Event::OpProgress { id, done, total } => {
                if let Some(op) = self.ops.iter_mut().find(|op| op.id == id) {
                    op.done = done;
                    op.total = total;
                }
            }
            Event::OpFinished {
                id,
                result,
                changed,
                select,
            } => {
                self.ops.retain(|op| op.id != id);
                match result {
                    Ok(message) if !message.is_empty() => self.toast(message, ToastKind::Success),
                    Ok(_) => {}
                    Err(error) if error == "Cancelled" => self.toast("Cancelled", ToastKind::Info),
                    Err(error) => self.toast(error, ToastKind::Error),
                }
                for (bucket, prefix) in &changed {
                    self.invalidate_tree(bucket, prefix.as_deref());
                }
                let here = self.location.bucket.clone();
                if changed
                    .iter()
                    .any(|(bucket, _)| Some(bucket) == here.as_ref())
                {
                    self.load_listing(true, select);
                }
            }
            Event::Dialog(reply) => self.dialog_reply(reply),
            Event::Toast { text, error } => {
                self.toast(
                    text,
                    if error {
                        ToastKind::Error
                    } else {
                        ToastKind::Info
                    },
                );
            }
            Event::Queued { count, verb } => {
                if count == 0 {
                    self.toast("Nothing to transfer", ToastKind::Info);
                } else {
                    self.toast(
                        format!("{verb} {}", format::plural(count, "file")),
                        ToastKind::Info,
                    );
                    self.show_transfers = true;
                }
            }
            Event::Update(reply) => self.update_reply(reply),
        }
    }

    fn transfers_changed(&mut self, changed: Vec<TransferView>) {
        let finished = |t: &TransferView| {
            matches!(t.status, Status::Done | Status::Failed | Status::Cancelled)
        };
        let was_idle = self.transfer_views.values().all(finished);
        if was_idle {
            if let Some(first) = changed.iter().filter(|t| !finished(t)).map(|t| t.id).min() {
                self.batch_start = first;
            }
        }
        let mut uploaded = HashSet::new();
        for transfer in changed {
            let was_done = self
                .transfer_views
                .get(&transfer.id)
                .is_some_and(|t| t.status == Status::Done);
            if transfer.kind == crate::transfers::Kind::Upload
                && transfer.status == Status::Done
                && !was_done
            {
                uploaded.insert(transfer.bucket.clone());
                let here = self.location.bucket.as_deref() == Some(transfer.bucket.as_str())
                    && self.active.as_deref() == Some(transfer.conn_id.as_str())
                    && transfer.key.starts_with(&self.location.prefix);
                if here {
                    // Coalesce a burst of finished uploads into one reload.
                    self.refresh_at = Some(Instant::now() + Duration::from_millis(800));
                }
            }
            self.transfer_views.insert(transfer.id, transfer);
        }
        if !uploaded.is_empty() {
            let mut buckets = self
                .tree_refresh_at
                .take()
                .map(|(_, b)| b)
                .unwrap_or_default();
            buckets.extend(uploaded);
            self.tree_refresh_at = Some((Instant::now() + Duration::from_millis(1500), buckets));
        }
    }

    fn run_timers(&mut self, ctx: &egui::Context) {
        self.check_for_updates_when_due(ctx);
        let now = Instant::now();
        if let Some(at) = self.refresh_at {
            if now >= at {
                self.refresh();
            } else {
                ctx.request_repaint_after(at - now);
            }
        }
        if let Some((at, _)) = &self.tree_refresh_at {
            if now >= *at {
                let (_, buckets) = self.tree_refresh_at.take().unwrap();
                for bucket in buckets {
                    self.invalidate_tree(&bucket, None);
                }
            } else {
                ctx.request_repaint_after(*at - now);
            }
        }
        self.toasts.retain(|toast| toast.until > now);
        if let Some(next) = self.toasts.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
        // Long listings are re-sorted a few times a second while they stream in.
        if self.view_dirty
            && (!self.listing.loading || self.view_built.elapsed() > Duration::from_millis(250))
        {
            self.rebuild_view();
        } else if self.view_dirty {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    // ----- input -------------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.viewer.is_some() {
            self.viewer_keys(ctx);
            return;
        }
        let command = Modifiers::COMMAND;
        let pressed = |ctx: &egui::Context, modifiers: Modifiers, key: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(modifiers, key)))
        };
        // These work even while typing in the filter box.
        if pressed(ctx, Modifiers::NONE, Key::F5) || pressed(ctx, command, Key::R) {
            self.refresh();
            return;
        }
        if pressed(ctx, command, Key::F) {
            self.focus_filter(ctx);
            return;
        }
        if self.modal.is_some() || ctx.egui_wants_keyboard_input() {
            return;
        }
        // An open menu gets the keyboard; Escape closes it.
        if egui::Popup::is_any_open(ctx) {
            if pressed(ctx, Modifiers::NONE, Key::Escape) {
                egui::Popup::close_all(ctx);
            }
            return;
        }
        let (back, forward) = ctx.input(|i| {
            (
                i.pointer.button_pressed(PointerButton::Extra1),
                i.pointer.button_pressed(PointerButton::Extra2),
            )
        });
        if back
            || pressed(ctx, Modifiers::ALT, Key::ArrowLeft)
            || pressed(ctx, command, Key::OpenBracket)
        {
            return self.go_back();
        }
        if forward
            || pressed(ctx, Modifiers::ALT, Key::ArrowRight)
            || pressed(ctx, command, Key::CloseBracket)
        {
            return self.go_forward();
        }
        if pressed(ctx, Modifiers::ALT, Key::ArrowUp)
            || pressed(ctx, command, Key::ArrowUp)
            || pressed(ctx, Modifiers::NONE, Key::Backspace)
        {
            return self.go_up();
        }
        let selected = self.selected_items();
        let single = (selected.len() == 1).then(|| selected[0].clone());

        if pressed(ctx, command, Key::A) {
            self.selection = self
                .view
                .iter()
                .map(|&i| self.listing.items[i].key.clone())
                .collect();
            return;
        }
        if pressed(ctx, command | Modifiers::SHIFT, Key::N) {
            return self.new_folder();
        }
        if pressed(ctx, command, Key::U) {
            return self.upload_pick(false);
        }
        if pressed(ctx, command, Key::L) {
            return self.copy_public_urls(&selected);
        }
        if pressed(ctx, Modifiers::NONE, Key::Delete) || pressed(ctx, command, Key::Backspace) {
            return self.confirm_delete(selected);
        }
        if pressed(ctx, Modifiers::NONE, Key::F2) {
            if let Some(item) = single {
                self.rename(&item);
            }
            return;
        }
        if pressed(ctx, Modifiers::NONE, Key::Escape) {
            self.selection.clear();
            return;
        }
        if pressed(ctx, Modifiers::NONE, Key::Enter) || pressed(ctx, command, Key::ArrowDown) {
            if let Some(item) = single {
                self.open_item(&item);
            }
            return;
        }
        let page = ((self.list_scroll.height / theme::ROW) as isize - 1).max(1);
        for (key, delta) in [
            (Key::ArrowDown, 1),
            (Key::ArrowUp, -1),
            (Key::PageDown, page),
            (Key::PageUp, -page),
            (Key::Home, isize::MIN),
            (Key::End, isize::MAX),
        ] {
            if pressed(ctx, Modifiers::NONE, key) {
                return self.move_cursor(delta, false);
            }
            if pressed(ctx, Modifiers::SHIFT, key) {
                return self.move_cursor(delta, true);
            }
        }

        // Clipboard shortcuts arrive as Copy/Cut/Paste events, typing as Text.
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Copy => self.copy_or_cut(&selected, ClipMode::Copy),
                egui::Event::Cut => self.copy_or_cut(&selected, ClipMode::Cut),
                egui::Event::Paste(_) => {
                    let prefix = self.location.prefix.clone();
                    self.paste(&prefix);
                }
                egui::Event::Text(text) if text == " " && self.typed.is_empty() => {
                    if let Some(item) = &single {
                        self.open_item(item);
                    }
                }
                egui::Event::Text(text) => self.type_ahead(&text),
                _ => {}
            }
        }
    }

    fn focus_filter(&mut self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique(panels::FILTER_ID)));
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        match (
            self.location.bucket.clone(),
            self.modal.is_none() && self.viewer.is_none(),
        ) {
            (Some(bucket), true) => {
                let prefix = self.location.prefix.clone();
                self.upload_paths(dropped, bucket, prefix);
            }
            _ => self.toast("Open a bucket to upload files", ToastKind::Info),
        }
    }
}

fn type_of(item: &Item) -> String {
    match item.kind {
        ItemKind::Bucket => "Bucket".into(),
        ItemKind::Folder => "Folder".into(),
        ItemKind::File => format::type_label(&item.name),
    }
}

impl eframe::App for FathomApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();
        self.run_timers(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_keys(&ctx);
        self.sync_preview(&ctx);

        let p = theme::palette(ui);
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(p.chrome)
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .exact_size(23.0)
            .frame(
                egui::Frame::new()
                    .fill(p.chrome)
                    .inner_margin(egui::Margin {
                        left: 8,
                        ..Default::default()
                    }),
            )
            .show(ui, |ui| self.status_bar(ui));
        if self.show_transfers {
            egui::Panel::bottom("transfers")
                .resizable(true)
                .default_size(200.0)
                .size_range(120.0..=480.0)
                .frame(egui::Frame::new().fill(p.bg))
                .show(ui, |ui| self.transfers_panel(ui));
        }
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(240.0)
            .size_range(180.0..=480.0)
            .frame(egui::Frame::new().fill(p.bg))
            .show(ui, |ui| self.sidebar(ui));
        if self.show_preview && self.active.is_some() {
            egui::Panel::right("preview")
                .resizable(true)
                .default_size(320.0)
                .size_range(240.0..=720.0)
                .frame(egui::Frame::new().fill(p.bg).inner_margin(10))
                .show(ui, |ui| self.preview_pane(ui));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.file_list(ui));

        self.viewer_overlay(&ctx);
        self.modals(&ctx);
        self.toasts_overlay(&ctx);
        self.drop_overlay(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let prefs = Prefs {
            show_preview: self.show_preview,
            show_transfers: self.show_transfers,
        };
        if let Ok(json) = serde_json::to_string(&prefs) {
            storage.set_string("prefs", json);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}
