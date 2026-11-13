#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use filesweep::{fmt_size, DupeGroup, FileEntry, Index};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(88, 166, 255);
const MUTED: egui::Color32 = egui::Color32::from_rgb(150, 158, 170);

enum Msg {
    ScanDone(Index),
    DupeDone(Vec<DupeGroup>, filesweep::DupeStats),

    IndexLoaded(Option<(filesweep::Index, u64, Vec<DupeGroup>)>),
    AnalyzeDone(Vec<filesweep::Suggestion>),

    Refreshed(Box<(filesweep::Index, Vec<filesweep::Suggestion>, usize)>),
    DupeProgress(usize, usize),
    DeleteProgress(usize, usize),
    DeleteDone(Box<DeleteOutcome>),
    PruneDone,
}

enum DeleteCtx {
    Dupes,
    Suggest(usize),

    Single,
}

#[derive(Clone, Copy, PartialEq)]
enum DeleteMode {

    Trash,

    Permanent,
}

struct DeleteOutcome {
    label: String,
    ctx: DeleteCtx,
    mode: DeleteMode,
    ok: Vec<PathBuf>,

    gone: Vec<PathBuf>,
    fail: usize,
    cancelled: bool,
    first_error: Option<String>,
}

#[cfg(target_os = "windows")]
mod com_init {
    use std::ffi::c_void;
    extern "system" {
        fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> i32;
        fn CoUninitialize();
    }

    pub fn init() -> bool {

        let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), 0x2) };
        hr == 0
    }
    pub fn uninit() {
        unsafe { CoUninitialize() }
    }
}

fn com_enter() -> bool {
    #[cfg(target_os = "windows")]
    {
        com_init::init()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

fn com_leave(inited: bool) {
    #[cfg(target_os = "windows")]
    if inited {
        com_init::uninit();
    }
    #[cfg(not(target_os = "windows"))]
    let _ = inited;
}

fn delete_worker(
    tx: Sender<Msg>,
    paths: Vec<PathBuf>,
    label: String,
    ctx: DeleteCtx,
    mode: DeleteMode,
    cancel: Arc<AtomicBool>,
    ectx: egui::Context,
) {
    if mode == DeleteMode::Permanent {

        let total = paths.len();
        let mut ok: Vec<PathBuf> = Vec::new();
        let mut gone: Vec<PathBuf> = Vec::new();
        let mut fail = 0usize;
        let mut done = 0usize;
        let mut cancelled = false;
        let mut first_error: Option<String> = None;
        for chunk in paths.chunks(1000) {
            if cancel.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            for p in chunk {
                match std::fs::remove_file(p) {
                    Ok(()) => ok.push(p.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        gone.push(p.clone())
                    }
                    Err(e) => {
                        fail += 1;
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                    }
                }
            }
            done += chunk.len();
            let _ = tx.send(Msg::DeleteProgress(done.min(total), total));
            ectx.request_repaint();
        }
        let _ = tx.send(Msg::DeleteDone(Box::new(DeleteOutcome {
            label,
            ctx,
            mode,
            ok,
            gone,
            fail,
            cancelled,
            first_error,
        })));
        ectx.request_repaint();
        return;
    }
    let com_inited = com_enter();
    const CHUNK: usize = 1000;
    let total = paths.len();
    let mut ok: Vec<PathBuf> = Vec::new();
    let mut gone: Vec<PathBuf> = Vec::new();
    let mut fail = 0usize;
    let mut done = 0usize;
    let mut cancelled = false;
    let mut first_error: Option<String> = None;
    for chunk in paths.chunks(CHUNK) {
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }

        let mut live: Vec<PathBuf> = Vec::with_capacity(chunk.len());
        for p in chunk {
            match std::fs::symlink_metadata(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    gone.push(p.clone())
                }
                _ => live.push(p.clone()),
            }
        }
        if !live.is_empty() {
            match trash::delete_all(live.iter()) {
                Ok(()) => ok.extend(live.iter().cloned()),
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                    for p in &live {
                        match trash::delete(p) {
                            Ok(()) => ok.push(p.clone()),
                            Err(e2) => {
                                fail += 1;
                                if first_error.is_none() {
                                    first_error = Some(e2.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        done += chunk.len();
        let _ = tx.send(Msg::DeleteProgress(done.min(total), total));
        ectx.request_repaint();
    }
    let _ = tx.send(Msg::DeleteDone(Box::new(DeleteOutcome {
        label,
        ctx,
        mode,
        ok,
        gone,
        fail,
        cancelled,
        first_error,
    })));
    ectx.request_repaint();
    com_leave(com_inited);
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Search,
    Dupes,
    Disk,
    Suggest,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum FCol {
    Name,
    Size,
    Date,
    Path,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum TCol {
    Dir,
    Size,
    Count,
}

#[derive(PartialEq, Clone, Copy)]
enum DSort {
    Wasted,
    Size,
    Count,
}

enum FileAction {
    Navigate(PathBuf),
    OpenExplorer(PathBuf),

    AskDelete(PathBuf, DeleteMode),
}

#[derive(Default)]
struct Cache {
    gen: u64,
    query: String,
    scol: Option<(FCol, bool)>,
    lcol: Option<(FCol, bool)>,
    tcol: Option<(TCol, bool)>,
    search: Vec<FileEntry>,
    largest: Vec<FileEntry>,
    toplevel: Vec<(PathBuf, u64, usize)>,
}

struct App {
    root_input: String,
    back_stack: Vec<PathBuf>,
    fwd_stack: Vec<PathBuf>,
    index: Option<Index>,
    gen: u64,
    dupes: Vec<DupeGroup>,
    dupes_stale: bool,
    dupe_sort: DSort,
    selected: HashSet<PathBuf>,
    query: String,
    tab: Tab,
    search_sort: (FCol, bool),
    largest_sort: (FCol, bool),
    toplevel_sort: (TCol, bool),
    cache: Cache,
    suggestions: Vec<filesweep::Suggestion>,
    suggest_confirm: Option<usize>,
    selected_suggest: usize,
    suggest_perm_arm: bool,
    dupes_perm_arm: bool,
    single_confirm: Option<(PathBuf, DeleteMode)>,
    skip_perm_confirm: bool,
    dont_ask_perm: bool,
    search_sel: HashMap<PathBuf, u64>,
    sugg_sel: HashMap<PathBuf, u64>,
    bulk_confirm: Option<(Vec<PathBuf>, u64, String, DeleteCtx, DeleteMode)>,
    prune_busy: bool,
    prune_pending: HashSet<PathBuf>,
    bg_tex: Option<egui::TextureHandle>,
    refresh_busy: bool,
    hide_protected: bool,
    dupe_skip_protected: bool,
    dupe_skip_small: bool,
    index_saved_at: u64,
    index_from_disk: bool,
    light_ui: bool,

    keeper_choice: HashMap<(u64, String), PathBuf>,
    deleted_paths: HashSet<PathBuf>,
    delete_progress: Option<(usize, usize)>,
    delete_label: String,
    cancel_delete: Arc<AtomicBool>,
    hash_progress: Option<(usize, usize)>,
    cancel_hash: Arc<AtomicBool>,
    egui_ctx: egui::Context,
    analyze_busy: bool,
    busy: Option<String>,
    status: String,
    confirm_delete: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_vietnamese_font(&cc.egui_ctx);
        let light0 = initial_light_ui();
        install_style(&cc.egui_ctx, light0);
        let (tx, rx) = std::sync::mpsc::channel();
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".to_string());
        let app = Self {
            root_input: home,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
            index: None,
            gen: 0,
            dupes: Vec::new(),
            dupes_stale: false,
            dupe_sort: DSort::Wasted,
            selected: HashSet::new(),
            query: String::new(),
            tab: Tab::Search,
            search_sort: (FCol::Name, true),
            largest_sort: (FCol::Size, false),
            toplevel_sort: (TCol::Size, false),
            cache: Cache::default(),
            suggestions: Vec::new(),
            suggest_confirm: None,
            selected_suggest: 0,
            suggest_perm_arm: false,
            dupes_perm_arm: false,
            single_confirm: None,
            skip_perm_confirm: load_setting("skip_perm_confirm").as_deref() == Some("1"),
            dont_ask_perm: false,
            search_sel: HashMap::new(),
            sugg_sel: HashMap::new(),
            bulk_confirm: None,
            prune_busy: false,
            prune_pending: HashSet::new(),
            bg_tex: None,
            refresh_busy: false,
            hide_protected: false,
            dupe_skip_protected: true,
            dupe_skip_small: true,
            index_saved_at: 0,
            index_from_disk: false,
            light_ui: light0,
            keeper_choice: HashMap::new(),
            deleted_paths: HashSet::new(),
            delete_progress: None,
            delete_label: String::new(),
            cancel_delete: Arc::new(AtomicBool::new(false)),
            hash_progress: None,
            cancel_hash: Arc::new(AtomicBool::new(false)),
            egui_ctx: cc.egui_ctx.clone(),
            analyze_busy: false,
            busy: None,
            status: "Pick a folder and press Scan to start.".to_string(),
            confirm_delete: false,
            tx,
            rx,
        };

        let tx2 = app.tx.clone();
        let ectx2 = app.egui_ctx.clone();
        thread::spawn(move || {
            let r = latest_index_file()
                .and_then(|f| filesweep::load_index(&f))
                .map(|(idx, at)| {
                    let d = filesweep::load_dupes(&dupes_file_for(&idx.root));
                    (idx, at, d)
                });
            let _ = tx2.send(Msg::IndexLoaded(r));
            ectx2.request_repaint();
        });
        app
    }

    fn current_root(&self) -> PathBuf {
        PathBuf::from(self.root_input.trim())
    }

    fn start_scan(&mut self) {
        let root = self.current_root();
        if !root.is_dir() {
            self.status = format!("Folder does not exist: {}", root.display());
            return;
        }
        self.busy = Some(format!("Scanning {} ...", root.display()));
        self.confirm_delete = false;
        self.search_sel.clear();
        self.sugg_sel.clear();
        self.bulk_confirm = None;
        let tx = self.tx.clone();
        let ectx = self.egui_ctx.clone();
        thread::spawn(move || {
            let idx = filesweep::scan(&root);
            save_index_for(&idx);
            let _ = std::fs::remove_file(dupes_file_for(&root));
            let _ = tx.send(Msg::ScanDone(idx));
            ectx.request_repaint();
        });
    }

    fn navigate(&mut self, path: PathBuf) {
        let cur = self.current_root();
        if path == cur {
            self.start_scan();
            return;
        }
        self.back_stack.push(cur);
        self.fwd_stack.clear();
        self.root_input = path.display().to_string();
        self.start_scan();
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.back_stack.pop() {
            let cur = self.current_root();
            self.fwd_stack.push(cur);
            self.root_input = prev.display().to_string();
            self.start_scan();
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.fwd_stack.pop() {
            let cur = self.current_root();
            self.back_stack.push(cur);
            self.root_input = next.display().to_string();
            self.start_scan();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_root().parent().map(|p| p.to_path_buf()) {
            if parent != self.current_root() {
                self.navigate(parent);
            }
        }
    }

    fn spawn_analyze(&mut self) {
        if let Some(ix) = &self.index {
            let entries = ix.entries.clone();
            let tx = self.tx.clone();
            let ectx = self.egui_ctx.clone();
            self.analyze_busy = true;
            thread::spawn(move || {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let s = filesweep::analyze_cleanup(&entries, now);
                let _ = tx.send(Msg::AnalyzeDone(s));
                ectx.request_repaint();
            });
        }
    }

    fn fast_refresh(&mut self) {
        if self.refresh_busy {
            return;
        }
        if self.deleted_paths.is_empty() {
            self.status =
                "No files have been deleted with the app in this session yet.".to_string();
            return;
        }
        let Some(mut idx) = self.index.take() else {
            return;
        };
        let removed = std::mem::take(&mut self.deleted_paths);
        let n = removed.len();
        self.refresh_busy = true;
        self.analyze_busy = true;
        self.status = format!(
            "Quick refresh running in the background (removing {} deleted files) — the UI stays responsive...",
            fmt_num(n)
        );
        let tx = self.tx.clone();
        let ectx = self.egui_ctx.clone();
        thread::spawn(move || {
            filesweep::refresh_index(&mut idx, &removed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let s = filesweep::analyze_cleanup(&idx.entries, now);
            save_index_for(&idx);
            let _ = tx.send(Msg::Refreshed(Box::new((idx, s, n))));
            ectx.request_repaint();
        });
    }

    fn start_dupes(&mut self) {
        let Some(idx) = &self.index else {
            self.status = "No data yet — run a Scan first.".to_string();
            return;
        };
        let entries: Vec<FileEntry> = idx.entries.clone();
        self.busy = Some("Hashing for duplicates (parallel threads + cache)...".to_string());
        self.cancel_hash.store(false, Ordering::SeqCst);
        self.hash_progress = Some((0, 0));
        let tx = self.tx.clone();
        let ectx = self.egui_ctx.clone();
        let cancel = self.cancel_hash.clone();
        let opts = filesweep::DupeOptions {
            min_size: if self.dupe_skip_small { 4096 } else { 0 },
            skip_protected: self.dupe_skip_protected,
            threads: 0,
        };
        thread::spawn(move || {
            let cache_file = hash_cache_path();
            let cache = filesweep::load_hash_cache(&cache_file);
            let started = std::time::Instant::now();
            let last_sent = std::sync::atomic::AtomicU64::new(0);
            let tx2 = tx.clone();
            let ectx2 = ectx.clone();
            let (groups, stats, cache) = filesweep::find_duplicates_fast(
                &entries,
                &opts,
                Some(cache),
                &cancel,
                &|done, total| {

                    let elapsed = started.elapsed().as_millis() as u64;
                    let last = last_sent.load(Ordering::Relaxed);
                    if elapsed - last >= 300 || done >= total {
                        last_sent.store(elapsed, Ordering::Relaxed);
                        let _ = tx2.send(Msg::DupeProgress(done, total));
                        ectx2.request_repaint();
                    }
                },
            );
            if let Some(parent) = cache_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = filesweep::save_hash_cache(&cache_file, &cache);
            let _ = tx.send(Msg::DupeDone(groups, stats));
            ectx.request_repaint();
        });
    }

    fn sort_dupes(&mut self) {
        match self.dupe_sort {
            DSort::Wasted => self.dupes.sort_by(|a, b| b.wasted().cmp(&a.wasted())),
            DSort::Size => self.dupes.sort_by(|a, b| b.size.cmp(&a.size)),
            DSort::Count => self
                .dupes
                .sort_by(|a, b| b.paths.len().cmp(&a.paths.len())),
        }
    }

    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::IndexLoaded(saved) => {
                    if let Some((idx, at, saved_dupes)) = saved {
                        if self.index.is_none() && self.busy.is_none() && !self.refresh_busy {
                            let n_saved = saved_dupes.len();
                            self.status = format!(
                                "Loaded the list scanned {} ({}): {} files, {} — ready instantly, no rescan needed. Press Scan to rescan from scratch.{}",
                                fmt_age(at),
                                idx.root.display(),
                                fmt_num(idx.entries.len()),
                                fmt_size(idx.total_bytes),
                                if n_saved > 0 {
                                    format!(" Includes {} saved duplicate groups from the previous run — press Find duplicates to recompute fresh numbers.", fmt_num(n_saved))
                                } else {
                                    String::new()
                                }
                            );
                            self.root_input = idx.root.display().to_string();
                            self.index = Some(idx);
                            self.index_saved_at = at;
                            self.index_from_disk = true;
                            if n_saved > 0 {
                                self.dupes = saved_dupes;
                                self.dupes_stale = false;
                            } else {
                                self.dupes_stale = true;
                            }
                            self.spawn_analyze();
                            self.gen += 1;
                        }
                    }
                }
                Msg::ScanDone(idx) => {
                    self.index_saved_at = now_secs();
                    self.index_from_disk = false;
                    self.status = format!(
                        "Scanned {}: {} files, {}, in {} ms.",
                        idx.root.display(),
                        fmt_num(idx.entries.len()),
                        fmt_size(idx.total_bytes),
                        idx.scan_ms
                    );
                    self.index = Some(idx);
                    self.deleted_paths.clear();
                    self.keeper_choice.clear();
                    self.selected_suggest = 0;

                    self.spawn_analyze();
                    self.gen += 1;
                    self.dupes.clear();
                    self.selected.clear();
                    self.dupes_stale = true;
                    self.busy = None;
                }
                Msg::DupeProgress(d, t) => {
                    self.hash_progress = Some((d, t));
                }
                Msg::DeleteProgress(d, t) => {
                    self.delete_progress = Some((d, t));
                }
                Msg::DeleteDone(out) => {
                    self.apply_delete_done(*out);
                }
                Msg::PruneDone => {
                    self.prune_busy = false;
                    self.kick_prune();
                }
                Msg::AnalyzeDone(s) => {
                    self.suggestions = s;
                    self.suggest_confirm = None;
                    self.analyze_busy = false;
                }
                Msg::Refreshed(boxed) => {
                    let (idx, s, n) = *boxed;
                    self.index_saved_at = now_secs();
                    self.status = format!(
                        "Quick refresh done: removed {} deleted files — {} files left, {}. No disk rescan, no rehashing.",
                        fmt_num(n),
                        fmt_num(idx.entries.len()),
                        fmt_size(idx.total_bytes)
                    );
                    self.index = Some(idx);
                    self.suggestions = s;
                    self.refresh_busy = false;
                    self.analyze_busy = false;
                    self.gen += 1;

                    if !self.dupes.is_empty() {
                        let gone = self.deleted_paths.clone();
                        for g in self.dupes.iter_mut() {
                            g.paths.retain(|p| !gone.contains(p));
                            if g.distinct_files > g.paths.len() {
                                g.distinct_files = g.paths.len();
                            }
                        }
                        self.dupes.retain(|g| g.paths.len() > 1);
                        self.dupes_stale = false;
                        let root = self.index.as_ref().unwrap().root.clone();
                        let groups = self.dupes.clone();
                        thread::spawn(move || save_dupes_for(&root, &groups));
                        if !self.dupes.is_empty() {
                            self.status += &format!(
                                " Duplicate groups were filtered for deleted files — {} groups remain.",
                                fmt_num(self.dupes.len())
                            );
                        }
                    }
                    self.selected.clear();
                    self.keeper_choice.clear();
                    self.confirm_delete = false;
                    self.dupes_perm_arm = false;
                    self.suggest_confirm = None;
                    self.suggest_perm_arm = false;
                    self.single_confirm = None;
                }
                Msg::DupeDone(groups, st) => {
                    self.hash_progress = None;
                    let wasted: u64 = groups.iter().map(|g| g.wasted()).sum();
                    let was_cancelled = self.cancel_hash.load(Ordering::SeqCst);
                    self.status = format!(
                        "{}Done: {} duplicate groups, {} wasted in total. {} candidate files: {} newly hashed, {} from cache, {} skipped (small/protected zone) — took {:.0} s.",
                        if was_cancelled {
                            "Cancelled midway — these are partial results. "
                        } else {
                            ""
                        },
                        groups.len(),
                        fmt_size(wasted),
                        fmt_num(st.candidates),
                        fmt_num(st.hashed),
                        fmt_num(st.cache_hits),
                        fmt_num(st.skipped),
                        st.secs
                    );
                    self.dupes = groups;
                    self.sort_dupes();
                    self.dupes_stale = false;
                    self.keeper_choice.clear();

                    {
                        let root = self
                            .index
                            .as_ref()
                            .map(|i| i.root.clone())
                            .unwrap_or_else(|| self.current_root());
                        let groups = self.dupes.clone();
                        thread::spawn(move || save_dupes_for(&root, &groups));
                    }
                    self.selected.clear();
                    self.confirm_delete = false;
                    self.busy = None;
                    if st.hardlink_groups > 0 {
                        self.status += &format!(
                            " Of these, {} groups are hardlinks ({} looks wasted but is the same physical data) — excluded from the wasted total.",
                            fmt_num(st.hardlink_groups),
                            fmt_size(st.hardlink_bytes)
                        );
                    }
                }
            }
        }
    }

    fn start_delete(&mut self, paths: Vec<PathBuf>, label: String, ctx: DeleteCtx, mode: DeleteMode) {
        if paths.is_empty() || self.delete_progress.is_some() {
            return;
        }
        self.cancel_delete.store(false, Ordering::SeqCst);
        self.delete_progress = Some((0, paths.len()));
        self.delete_label = label.clone();
        self.status = format!(
            "Deleting {} files ({}){}...",
            fmt_num(paths.len()),
            label,
            if mode == DeleteMode::Permanent {
                " — PERMANENTLY"
            } else {
                ""
            }
        );
        let tx = self.tx.clone();
        let cancel = self.cancel_delete.clone();
        let ectx = self.egui_ctx.clone();
        thread::spawn(move || delete_worker(tx, paths, label, ctx, mode, cancel, ectx));
    }

    fn kick_prune(&mut self) {
        if self.prune_busy || self.prune_pending.is_empty() {
            return;
        }
        let Some(idx) = &self.index else {
            return;
        };
        let root = idx.root.clone();
        let removed: HashSet<PathBuf> = std::mem::take(&mut self.prune_pending);
        self.prune_busy = true;
        let tx = self.tx.clone();
        let ectx = self.egui_ctx.clone();
        thread::spawn(move || {
            prune_saved_index(&root, &removed);
            let _ = tx.send(Msg::PruneDone);
            ectx.request_repaint();
        });
    }

    fn apply_delete_done(&mut self, out: DeleteOutcome) {
        self.delete_progress = None;

        let mut removed: HashSet<PathBuf> = out.ok.iter().cloned().collect();
        removed.extend(out.gone.iter().cloned());
        self.search_sel.retain(|p, _| !removed.contains(p));
        self.sugg_sel.retain(|p, _| !removed.contains(p));
        for p in &removed {
            self.deleted_paths.insert(p.clone());
        }
        match out.ctx {
            DeleteCtx::Single => {}
            DeleteCtx::Dupes => {
                for g in self.dupes.iter_mut() {
                    g.paths.retain(|p| !removed.contains(p));
                }
                self.dupes.retain(|g| g.paths.len() > 1);
                self.selected.clear();
                self.confirm_delete = false;
            }
            DeleteCtx::Suggest(i) => {
                if let Some(s) = self.suggestions.get_mut(i) {
                    s.entries.retain(|e| !removed.contains(&e.path));
                }
            }
        }
        let instant = !removed.is_empty() && removed.len() <= 5000;
        if !removed.is_empty() {
            self.dupes_stale = true;
        }
        if instant {
            let mut removed_bytes: u64 = 0;
            let mut top_delta: HashMap<PathBuf, (u64, usize)> = HashMap::new();
            if let Some(idx) = &mut self.index {
                let root = idx.root.clone();
                idx.entries.retain(|e| {
                    if removed.contains(&e.path) {
                        removed_bytes += e.size;
                        let key = match e.path.strip_prefix(&root) {
                            Ok(rel) => {
                                let mut comps = rel.components();
                                match comps.next() {
                                    Some(c) if rel.components().count() > 1 => {
                                        root.join(c.as_os_str())
                                    }
                                    _ => root.clone(),
                                }
                            }
                            Err(_) => root.clone(),
                        };
                        let slot = top_delta.entry(key).or_default();
                        slot.0 += e.size;
                        slot.1 += 1;
                        false
                    } else {
                        true
                    }
                });
                idx.total_bytes = idx.total_bytes.saturating_sub(removed_bytes);
            }
            self.gen += 1;
            if !matches!(out.ctx, DeleteCtx::Dupes) && !self.dupes.is_empty() {
                for g in self.dupes.iter_mut() {
                    g.paths.retain(|p| !removed.contains(p));
                    if g.distinct_files > g.paths.len() {
                        g.distinct_files = g.paths.len();
                    }
                }
                self.dupes.retain(|g| g.paths.len() > 1);
            }

            for s in self.suggestions.iter_mut() {
                s.entries.retain(|e| !removed.contains(&e.path));
            }
            self.cache.search.retain(|e| !removed.contains(&e.path));
            self.cache.largest.retain(|e| !removed.contains(&e.path));
            if !top_delta.is_empty() {
                for row in self.cache.toplevel.iter_mut() {
                    if let Some((b, n)) = top_delta.get(&row.0) {
                        row.1 = row.1.saturating_sub(*b);
                        row.2 = row.2.saturating_sub(*n);
                    }
                }
                self.cache.toplevel.retain(|r| r.2 > 0);
            }
            self.cache.gen = self.gen;
            self.cache.query = self.query.clone();
            self.cache.scol = Some(self.search_sort);
            self.cache.lcol = Some(self.largest_sort);
            self.cache.tcol = Some(self.toplevel_sort);
            self.prune_pending.extend(removed.iter().cloned());
            self.kick_prune();
        }
        self.status = format!(
            "{} {} {} ({}){}{}{}",
            if out.mode == DeleteMode::Permanent {
                "Permanently deleted"
            } else {
                "Moved to the Recycle Bin"
            },
            if out.ok.len() == 1 { "file" } else { "files" },
            fmt_num(out.ok.len()),
            out.label,
            if !out.gone.is_empty() {
                format!(
                    " — {} files were already gone from disk and have been removed from the list",
                    fmt_num(out.gone.len())
                )
            } else {
                String::new()
            },
            if out.fail > 0 {
                let hint = match &out.first_error {
                    Some(fe)
                        if (fe.contains("Access is denied") || fe.contains("os error 5"))
                            && !is_elevated() =>
                    {
                        " — Windows blocked it: the file sits in a protected location. Restart the app as administrator (sidebar) to delete files there."
                    }
                    _ => "",
                };
                match &out.first_error {
                    Some(fe) => format!(
                        " — {} files failed; first error: {}{}",
                        out.fail, fe, hint
                    ),
                    None => format!(" — {} files failed{}", out.fail, hint),
                }
            } else {
                String::new()
            },
            if out.cancelled {
                " — CANCELLED midway, the rest was not deleted"
            } else if instant {
                ". The list has been updated."
            } else if !removed.is_empty() {
                ". Press ↻ Quick refresh in the sidebar to sync the numbers."
            } else {
                ""
            }
        );
    }

    fn delete_selected_mode(&mut self, mode: DeleteMode) {
        let targets: Vec<PathBuf> = self.selected.iter().cloned().collect();
        self.confirm_delete = false;
        self.dupes_perm_arm = false;
        self.start_delete(targets, "selected duplicates".to_string(), DeleteCtx::Dupes, mode);
    }

    fn refresh_cache(&mut self) {
        let Some(idx) = &self.index else {
            return;
        };
        let c = &mut self.cache;
        if c.gen == self.gen
            && c.query == self.query
            && c.scol == Some(self.search_sort)
            && c.lcol == Some(self.largest_sort)
            && c.tcol == Some(self.toplevel_sort)
        {
            return;
        }

        let mut search: Vec<FileEntry> = filesweep::search(idx, &self.query, 20_000)
            .into_iter()
            .cloned()
            .collect();
        sort_files(&mut search, self.search_sort);

        let mut largest: Vec<FileEntry> = {
            let mut refs: Vec<&FileEntry> = idx.entries.iter().collect();
            if refs.len() > 200 {
                let (col, asc) = self.largest_sort;
                refs.select_nth_unstable_by(199, |a, b| {
                    let ord = match col {
                        FCol::Name => a.name_lower.cmp(&b.name_lower),
                        FCol::Size => a.size.cmp(&b.size),
                        FCol::Date => a.modified_secs.cmp(&b.modified_secs),
                        FCol::Path => a.path_lower.cmp(&b.path_lower),
                    };
                    if asc { ord } else { ord.reverse() }
                });
                refs.truncate(200);
            }
            let mut v: Vec<FileEntry> = refs.into_iter().cloned().collect();
            sort_files(&mut v, self.largest_sort);
            v
        };

        let mut toplevel = filesweep::top_level_sizes(idx);
        let (tc, tasc) = self.toplevel_sort;
        toplevel.sort_by(|a, b| {
            let ord = match tc {
                TCol::Dir => a.0.cmp(&b.0),
                TCol::Size => a.1.cmp(&b.1),
                TCol::Count => a.2.cmp(&b.2),
            };
            if tasc {
                ord
            } else {
                ord.reverse()
            }
        });
        self.cache = Cache {
            gen: self.gen,
            query: self.query.clone(),
            scol: Some(self.search_sort),
            lcol: Some(self.largest_sort),
            tcol: Some(self.toplevel_sort),
            search,
            largest,
            toplevel,
        };
    }
}

fn sort_files(v: &mut [FileEntry], (col, asc): (FCol, bool)) {
    v.sort_by(|a, b| {
        let ord = match col {
            FCol::Name => a.name_lower.cmp(&b.name_lower),
            FCol::Size => a.size.cmp(&b.size),
            FCol::Date => a.modified_secs.cmp(&b.modified_secs),
            FCol::Path => a.path_lower.cmp(&b.path_lower),
        };
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

fn fmt_date(secs: u64) -> String {
    if secs == 0 {
        return "—".to_string();
    }
    let days = (secs / 86_400) as i64;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn install_vietnamese_font(ctx: &egui::Context) {
    let bytes = include_bytes!("../assets/NotoSans-Regular.ttf");
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("noto".to_owned(), egui::FontData::from_static(bytes));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "noto".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn make_bg_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let (w, h) = (512usize, 320usize);
    let top = [24.0f32, 36.0, 51.0];
    let bottom = [10.0f32, 14.0, 21.0];
    let aspect = 1180.0f32 / 760.0;
    let glows: [((f32, f32), f32, [f32; 3], f32); 2] = [
        ((0.94, -0.07), 0.395, [80.0, 180.0, 255.0], 5.0 / 255.0),
        ((0.03, 1.09), 0.316, [64.0, 224.0, 190.0], 4.0 / 255.0),
    ];
    let mut px = vec![0u8; w * h * 3];
    for y in 0..h {
        let v = y as f32 / (h - 1) as f32;
        for x in 0..w {
            let u = x as f32 / (w - 1) as f32;
            let mut c = [
                top[0] + (bottom[0] - top[0]) * v,
                top[1] + (bottom[1] - top[1]) * v,
                top[2] + (bottom[2] - top[2]) * v,
            ];
            for ((cx, cy), r, col, a) in &glows {
                let dx = (u - cx) * aspect;
                let dy = v - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let t = (1.0 - d / r).clamp(0.0, 1.0);
                let alpha = a * t * t;
                for i in 0..3 {
                    c[i] = c[i] * (1.0 - alpha) + col[i] * alpha;
                }
            }
            let o = (y * w + x) * 3;
            px[o] = c[0].clamp(0.0, 255.0) as u8;
            px[o + 1] = c[1].clamp(0.0, 255.0) as u8;
            px[o + 2] = c[2].clamp(0.0, 255.0) as u8;
        }
    }
    let img = egui::ColorImage::from_rgb([w, h], &px);
    ctx.load_texture("filesweep-bg", img, egui::TextureOptions::LINEAR)
}

fn install_style(ctx: &egui::Context, light: bool) {
    let mut style = (*ctx.style()).clone();
    use egui::{FontId, TextStyle};
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(17.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.0));
    style.spacing.item_spacing = egui::Vec2::new(6.0, 4.0);
    style.spacing.button_padding = egui::Vec2::new(9.0, 4.0);
    style.animation_time = 0.0;

    let mut v = egui::Visuals::dark();
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    let glass_stroke = egui::Stroke::new(
        1.0,
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 34),
    );
    let r10 = egui::Rounding::same(10.0);
    if light {

        v.panel_fill = egui::Color32::from_rgb(17, 22, 31);
        v.window_fill = egui::Color32::from_rgb(20, 26, 37);
        v.faint_bg_color = egui::Color32::from_rgb(28, 35, 47);
        v.extreme_bg_color = egui::Color32::from_rgb(11, 14, 20);
        v.selection.bg_fill = egui::Color32::from_rgb(30, 90, 170);
        v.override_text_color = Some(egui::Color32::from_rgb(229, 237, 247));
        v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 30, 41);
        v.widgets.noninteractive.weak_bg_fill = egui::Color32::from_rgb(21, 27, 36);
        v.widgets.noninteractive.rounding = r10;
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(38, 46, 59);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(31, 38, 49);
        v.widgets.inactive.rounding = r10;

        v.widgets.hovered.fg_stroke = v.widgets.inactive.fg_stroke;
        v.widgets.open.fg_stroke = v.widgets.inactive.fg_stroke;
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 46, 59);
        v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(31, 38, 49);
        v.widgets.hovered.rounding = r10;
        v.widgets.active.bg_fill = egui::Color32::from_rgb(30, 90, 170);
        v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(26, 78, 148);
        v.widgets.active.rounding = r10;
        v.widgets.open.bg_fill = egui::Color32::from_rgb(38, 46, 59);
        v.widgets.open.weak_bg_fill = egui::Color32::from_rgb(31, 38, 49);
        v.widgets.open.rounding = r10;
        style.visuals = v;
        ctx.set_style(style);
        return;
    }
    v.panel_fill = egui::Color32::from_rgba_unmultiplied(15, 21, 32, 238);
    v.window_fill = egui::Color32::from_rgba_unmultiplied(18, 25, 38, 245);
    v.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 9);
    v.extreme_bg_color = egui::Color32::from_rgb(9, 13, 19);
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(56, 150, 248, 190);
    v.override_text_color = Some(egui::Color32::from_rgb(229, 237, 247));
    v.widgets.noninteractive.bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 7);
    v.widgets.noninteractive.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 5);
    v.widgets.noninteractive.bg_stroke = glass_stroke;
    v.widgets.noninteractive.rounding = r10;
    v.widgets.inactive.bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14);
    v.widgets.inactive.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 9);
    v.widgets.inactive.bg_stroke = glass_stroke;
    v.widgets.inactive.rounding = r10;
    v.widgets.hovered.bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 27);
    v.widgets.hovered.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(
        1.0,
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60),
    );
    v.widgets.hovered.rounding = r10;
    v.widgets.active.bg_fill =
        egui::Color32::from_rgba_unmultiplied(56, 150, 248, 130);
    v.widgets.active.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(56, 150, 248, 90);
    v.widgets.active.bg_stroke = egui::Stroke::new(
        1.0,
        egui::Color32::from_rgba_unmultiplied(140, 205, 255, 120),
    );
    v.widgets.active.rounding = r10;
    v.widgets.open.bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18);
    v.widgets.open.weak_bg_fill =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12);
    v.widgets.open.bg_stroke = glass_stroke;
    v.widgets.open.rounding = r10;
    v.window_rounding = egui::Rounding::same(14.0);
    v.menu_rounding = egui::Rounding::same(10.0);
    style.visuals = v;
    ctx.set_style(style);
}

fn sort_arrow(asc: bool) -> &'static str {
    if asc {
        " ↑"
    } else {
        " ↓"
    }
}

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

fn file_table(
    ui: &mut egui::Ui,
    rows: &[FileEntry],
    sort: &mut (FCol, bool),
    max_height: Option<f32>,
    sel: &mut HashMap<PathBuf, u64>,
) -> (Option<FileAction>, bool) {
    let mut action: Option<FileAction> = None;
    let mut sort_changed = false;
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::initial(26.0).range(24.0..=30.0))
        .column(Column::initial(250.0).range(110.0..=800.0).clip(true))
        .column(Column::initial(92.0).range(70.0..=160.0))
        .column(Column::initial(92.0).range(76.0..=140.0))
        .column(Column::remainder().clip(true).resizable(false))
        .column(Column::initial(196.0).range(170.0..=230.0));
    if let Some(h) = max_height {
        table = table.max_scroll_height(h);
    }
    table
        .header(24.0, |mut header| {
            header.col(|ui| {
                let all = !rows.is_empty() && rows.iter().all(|e| sel.contains_key(&e.path));
                let mut all_on = all;
                if ui
                    .checkbox(&mut all_on, "")
                    .on_hover_text("Select / unselect all files in this list")
                    .changed()
                {
                    if all_on {
                        for e in rows {
                            sel.insert(e.path.clone(), e.size);
                        }
                    } else {
                        for e in rows {
                            sel.remove(&e.path);
                        }
                    }
                }
            });
            for (col, label) in [
                (FCol::Name, "File name"),
                (FCol::Size, "Size"),
                (FCol::Date, "Modified"),
                (FCol::Path, "Path"),
            ] {
                header.col(|ui| {
                    let active = sort.0 == col;
                    let text = if active {
                        format!("{}{}", label, sort_arrow(sort.1))
                    } else {
                        label.to_string()
                    };
                    let mut rt = egui::RichText::new(text).strong();
                    if active {
                        rt = rt.color(ACCENT);
                    }
                    if ui.button(rt).on_hover_text("Click to sort this column").clicked() {
                        if active {
                            sort.1 = !sort.1;
                        } else {
                            *sort = (col, col != FCol::Size && col != FCol::Date);
                        }
                        sort_changed = true;
                    }
                });
            }
            header.col(|ui| {
                ui.label(egui::RichText::new("Actions").strong());
            });
        })
        .body(|body| {
            body.rows(22.0, rows.len(), |mut row| {
                let e = &rows[row.index()];
                row.col(|ui| {
                    let mut on = sel.contains_key(&e.path);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            sel.insert(e.path.clone(), e.size);
                        } else {
                            sel.remove(&e.path);
                        }
                    }
                });
                row.col(|ui| {
                    ui.label(&e.name);
                });
                row.col(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(fmt_size(e.size));
                    });
                });
                row.col(|ui| {
                    ui.label(fmt_date(e.modified_secs));
                });
                row.col(|ui| {
                    ui.label(egui::RichText::new(e.path.display().to_string()).color(MUTED))
                        .on_hover_text(e.path.display().to_string());
                });
                row.col(|ui| {
                    if ui
                        .small_button("Open")
                        .on_hover_text("Jump to the folder containing this file, right in the app")
                        .clicked()
                    {
                        if let Some(parent) = e.path.parent() {
                            action = Some(FileAction::Navigate(parent.to_path_buf()));
                        }
                    }
                    if ui
                        .small_button("↗ Open")
                        .on_hover_text("Open the containing folder in Explorer")
                        .clicked()
                    {
                        if let Some(parent) = e.path.parent() {
                            action = Some(FileAction::OpenExplorer(parent.to_path_buf()));
                        }
                    }
                    if ui
                        .small_button("🗑")
                        .on_hover_text("Move this file to the Recycle Bin")
                        .clicked()
                    {
                        action = Some(FileAction::AskDelete(e.path.clone(), DeleteMode::Trash));
                    }
                    if ui
                        .small_button("⚡")
                        .on_hover_text("Permanently delete this file (fast, not recoverable)")
                        .clicked()
                    {
                        action = Some(FileAction::AskDelete(
                            e.path.clone(),
                            DeleteMode::Permanent,
                        ));
                    }
                });
            });
        });
    (action, sort_changed)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.busy.is_some()
            || self.delete_progress.is_some()
            || self.hash_progress.is_some()
            || self.analyze_busy
            || self.refresh_busy
        {

            ctx.request_repaint_after(std::time::Duration::from_millis(400));
        }
        self.refresh_cache();

        let mut do_scan = false;
        let mut do_fast_refresh = false;
        let mut do_back = false;
        let mut do_forward = false;
        let mut do_up = false;
        let mut breadcrumb_target: Option<PathBuf> = None;

        egui::SidePanel::left("nav")
            .resizable(false)
            .default_width(214.0)
            .width_range(214.0..=214.0)
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgba_unmultiplied(16, 23, 35, 245))
                    .inner_margin(egui::Margin::same(12.0)),
            )
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("📁 FileSweep")
                        .size(19.0)
                        .strong()
                        .color(ACCENT),
                );
                ui.label(
                    egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(MUTED),
                );
                ui.add_space(12.0);

                for (t, label) in [
                    (Tab::Search, "🔎   Search"),
                    (Tab::Dupes, "👯   Duplicates"),
                    (Tab::Disk, "💽   Disk"),
                    (Tab::Suggest, "💡   Cleanup tips"),
                ] {
                    let selected = self.tab == t;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::Vec2::new(ui.available_width(), 36.0),
                        egui::Sense::click(),
                    );
                    let fill = if selected {
                        egui::Color32::from_rgba_unmultiplied(56, 150, 248, 165)
                    } else if resp.hovered() && !self.light_ui {
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10)
                    };
                    let painter = ui.painter();
                    painter.rect_filled(rect, egui::Rounding::same(10.0), fill);
                    painter.text(
                        egui::pos2(rect.left() + 12.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgb(235, 242, 250),
                    );
                    if resp.clicked() {
                        self.tab = t;
                    }
                    ui.add_space(4.0);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Root folder").strong());
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.root_input)
                        .desired_width(ui.available_width())
                        .hint_text("Example: C:\\ or D:\\"),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Choose...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            let cur = self.current_root();
                            if p != cur {
                                self.back_stack.push(cur);
                                self.fwd_stack.clear();
                            }
                            self.root_input = p.display().to_string();
                            do_scan = true;
                        }
                    }
                    if ui
                        .add_enabled(
                            self.busy.is_none() && !self.refresh_busy,
                            egui::Button::new("🔍 Scan"),
                        )
                        .clicked()
                    {
                        do_scan = true;
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_scan = true;
                    }
                });
                ui.add_space(4.0);
                if ui
                    .add_enabled(
                        !self.deleted_paths.is_empty() && self.busy.is_none(),
                        egui::Button::new(format!(
                            "↻ Quick refresh ({})",
                            fmt_num(self.deleted_paths.len())
                        )),
                    )
                    .on_hover_text("Removes files you deleted with the app from the displayed data — no disk rescan. Files deleted in Explorer still need a full rescan.")
                    .clicked()
                {
                    do_fast_refresh = true;
                }
                if let Some(b) = &self.busy {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(b.clone());
                    });
                }
                if self.refresh_busy {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Quick refreshing...");
                    });
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                let mut light = self.light_ui;
                if ui
                    .checkbox(&mut light, "Light UI")
                    .on_hover_text("Flat background, no glow spots and no translucency: cheaper to repaint, less flicker when moving the mouse — especially over remote desktop. Your choice is remembered for the next launch.")
                    .changed()
                {
                    self.light_ui = light;
                    install_style(ctx, light);
                    save_light_ui(light);
                }
                let mut ask_perm = !self.skip_perm_confirm;
                if ui
                    .checkbox(&mut ask_perm, "Ask before permanent delete")
                    .on_hover_text("When unticked, the \u{26a1} permanent-delete buttons delete immediately, with no confirmation step. Deletes to the Recycle Bin are unaffected. Tick it again here any time to bring the confirmation back.")
                    .changed()
                {
                    self.skip_perm_confirm = !ask_perm;
                    save_setting(
                        "skip_perm_confirm",
                        if self.skip_perm_confirm { "1" } else { "0" },
                    );
                }
                if !is_elevated() {
                    ui.add_space(4.0);
                    if ui
                        .button("\u{1f6e1} Restart as administrator")
                        .on_hover_text("Deleting files inside C:\\Windows, Program Files and other protected folders needs an elevated token. This restarts FileSweep with one (Windows asks once via UAC).")
                        .clicked()
                    {
                        if relaunch_elevated() {
                            std::process::exit(0);
                        } else {
                            self.status =
                                "Could not restart as administrator (UAC declined?).".to_string();
                        }
                    }
                }
                if let Some(idx) = &self.index {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} files · {}",
                            fmt_num(idx.entries.len()),
                            fmt_size(idx.total_bytes)
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                    ui.label(
                        egui::RichText::new(if self.index_from_disk {
                            format!("Saved list · scanned in {} ms", idx.scan_ms)
                        } else {
                            format!("Scanned in {} ms", idx.scan_ms)
                        })
                        .size(12.0)
                        .color(MUTED),
                    );
                }
            });

        let mut cancel_clicked = false;
        let mut cancel_hash_clicked = false;
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(3.0);
            if let Some((done, total)) = self.hash_progress {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Finding duplicates (hashing): {} / {} files",
                        fmt_num(done),
                        fmt_num(total)
                    ));
                    let frac = if total > 0 {
                        done as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .show_percentage()
                            .desired_width(280.0),
                    );
                    if ui.button("⏹ Cancel").clicked() {
                        cancel_hash_clicked = true;
                    }
                });
            } else if let Some((done, total)) = self.delete_progress {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Deleting {}: {} / {} files",
                        self.delete_label,
                        fmt_num(done),
                        fmt_num(total)
                    ));
                    let frac = if total > 0 {
                        done as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .show_percentage()
                            .desired_width(280.0),
                    );
                    if ui.button("⏹ Cancel delete").clicked() {
                        cancel_clicked = true;
                    }
                });
            } else {
                ui.label(self.status.clone());
            }
            ui.add_space(3.0);
        });
        if cancel_clicked {
            self.cancel_delete.store(true, Ordering::SeqCst);
            self.status =
                "Cancelling delete... (waiting for the current batch, up to 1,000 files)".to_string();
        }
        if cancel_hash_clicked {
            self.cancel_hash.store(true, Ordering::SeqCst);
            self.status = "Cancelling duplicate search...".to_string();
        }

        let mut pending_action: Option<FileAction> = None;
        egui::CentralPanel::default().show(ctx, |ui| {

            if self.light_ui {
                let rect = ui.max_rect();
                ui.painter().rect_filled(
                    rect,
                    egui::Rounding::ZERO,
                    egui::Color32::from_rgb(15, 20, 29),
                );
            }
            if !self.light_ui {
                let rect = ui.max_rect();
                if self.bg_tex.is_none() {
                    self.bg_tex = Some(make_bg_texture(ctx));
                }
                if let Some(tex) = &self.bg_tex {
                    ui.painter().image(
                        tex.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
            }

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.back_stack.is_empty(), egui::Button::new("◀ Back"))
                    .clicked()
                {
                    do_back = true;
                }
                if ui
                    .add_enabled(!self.fwd_stack.is_empty(), egui::Button::new("Forward ▶"))
                    .clicked()
                {
                    do_forward = true;
                }
                if ui.button("⬆ Up one level").clicked() {
                    do_up = true;
                }
                ui.separator();
                let root = self.current_root();
                egui::ScrollArea::horizontal()
                    .id_salt("crumbs")
                    .max_width(ui.available_width() - 8.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            for (i, (label, path)) in breadcrumbs(&root).into_iter().enumerate() {
                                if i > 0 {
                                    ui.label(egui::RichText::new("›").color(MUTED));
                                }
                                if ui
                                    .add(egui::Button::new(label).frame(false))
                                    .on_hover_text(path.display().to_string())
                                    .clicked()
                                {
                                    breadcrumb_target = Some(path);
                                }
                            }
                        });
                    });
            });
            ui.add_space(2.0);
            self.ui_single_confirm(ui);
            self.ui_bulk_confirm(ui);
            match self.tab {
                Tab::Search => {
                    if let Some(a) = self.ui_search(ui) {
                        pending_action = Some(a);
                    }
                }
                Tab::Dupes => self.ui_dupes(ui),
                Tab::Suggest => self.ui_suggest(ui),
                Tab::Disk => {
                    if let Some(a) = self.ui_disk(ui) {
                        pending_action = Some(a);
                    }
                }
            }
        });
        if do_back {
            self.go_back();
        }
        if do_forward {
            self.go_forward();
        }
        if do_up {
            self.go_up();
        }
        if let Some(p) = breadcrumb_target {
            self.navigate(p);
        }
        if do_scan {
            self.start_scan();
        }
        if do_fast_refresh {
            self.fast_refresh();
        }

        match pending_action {
            Some(FileAction::Navigate(p)) => self.navigate(p),
            Some(FileAction::OpenExplorer(p)) => {
                let _ = open::that(&p);
            }
            Some(FileAction::AskDelete(p, m)) => {
                if m == DeleteMode::Permanent && self.skip_perm_confirm {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string());
                    self.start_delete(vec![p], name, DeleteCtx::Single, m);
                } else {
                    self.single_confirm = Some((p, m));
                }
            }
            None => {}
        }
    }
}

fn settings_file() -> PathBuf {
    data_dir().join("settings.txt")
}

fn load_setting(key: &str) -> Option<String> {
    let s = std::fs::read_to_string(settings_file()).ok()?;
    for line in s.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn save_setting(key: &str, value: &str) {
    let p = settings_file();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&p)
        .map(|s| {
            s.lines()
                .filter(|l| {
                    !l.trim().is_empty()
                        && l.split_once('=').map(|(k, _)| k.trim() != key).unwrap_or(true)
                })
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default();
    lines.push(format!("{key}={value}"));
    let _ = std::fs::write(p, lines.join("\n"));
}

#[cfg(windows)]
fn is_elevated() -> bool {
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    true
}

#[cfg(windows)]
fn relaunch_elevated() -> bool {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let op: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let r = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    r > 32
}

#[cfg(not(windows))]
fn relaunch_elevated() -> bool {
    false
}

fn is_remote_session() -> bool {
    if let Ok(sn) = std::env::var("SESSIONNAME") {
        if sn.to_lowercase().starts_with("rdp") {
            return true;
        }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn GetSystemMetrics(n_index: i32) -> i32;
        }
        unsafe {
            return GetSystemMetrics(0x1000) != 0;
        }
    }
    #[allow(unreachable_code)]
    false
}

fn initial_light_ui() -> bool {
    match load_setting("light_ui") {
        Some(v) => v == "1",
        None => is_remote_session(),
    }
}

fn save_light_ui(light: bool) {
    save_setting("light_ui", if light { "1" } else { "0" });
}

fn data_dir() -> PathBuf {
    if let Ok(mut p) = std::env::var("LOCALAPPDATA").map(PathBuf::from) {
        p.push("FileSweep");
        p
    } else {
        PathBuf::from(".")
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn index_file_for(root: &Path) -> PathBuf {
    let s = root.to_string_lossy().to_lowercase();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    data_dir().join(format!("index-{h:016x}.bin"))
}

fn latest_index_file() -> Option<PathBuf> {
    let dir = data_dir();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with("index-") && name.ends_with(".bin") {
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                if best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                    best = Some((m, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn dupes_file_for(root: &Path) -> PathBuf {
    let s = root.to_string_lossy().to_lowercase();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    data_dir().join(format!("dupes-{h:016x}.bin"))
}

fn prune_saved_index(root: &Path, removed: &HashSet<PathBuf>) {
    let f = index_file_for(root);
    if let Some((mut idx, at)) = filesweep::load_index(&f) {
        idx.entries.retain(|e| !removed.contains(&e.path));
        idx.total_bytes = idx.entries.iter().map(|e| e.size).sum();
        let _ = filesweep::save_index(&f, &idx, at);
    }
    let df = dupes_file_for(root);
    let mut groups = filesweep::load_dupes(&df);
    if !groups.is_empty() {
        for g in groups.iter_mut() {
            g.paths.retain(|p| !removed.contains(p));
            if g.distinct_files > g.paths.len() {
                g.distinct_files = g.paths.len();
            }
        }
        groups.retain(|g| g.paths.len() > 1);
        let _ = filesweep::save_dupes(&df, &groups);
    }
}

fn save_dupes_for(root: &Path, groups: &[DupeGroup]) {
    let _ = filesweep::save_dupes(&dupes_file_for(root), groups);
}

fn save_index_for(idx: &filesweep::Index) {
    let _ = filesweep::save_index(&index_file_for(&idx.root), idx, now_secs());
}

fn fmt_age(at: u64) -> String {
    let delta = now_secs().saturating_sub(at);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        format!("{} minutes ago", delta / 60)
    } else if delta < 86400 {
        format!("{} hours ago", delta / 3600)
    } else {
        format!("{} days ago", delta / 86400)
    }
}

fn hash_cache_path() -> PathBuf {
    if let Ok(mut p) = std::env::var("LOCALAPPDATA").map(PathBuf::from) {
        p.push("FileSweep");
        p.push("hash-cache.bin");
        p
    } else {
        PathBuf::from("timfile-hash-cache.bin")
    }
}

fn breadcrumbs(path: &std::path::Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut acc = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => {
                acc.push(p.as_os_str());
                out.push((p.as_os_str().to_string_lossy().to_string(), acc.clone()));
            }
            Component::RootDir => {
                acc.push(comp.as_os_str());
                if out.is_empty() {
                    out.push(("/".to_string(), acc.clone()));
                }
            }
            Component::Normal(name) => {
                acc.push(name);
                out.push((name.to_string_lossy().to_string(), acc.clone()));
            }
            _ => {}
        }
    }
    out
}

fn card_frame(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui)) {
    let fill = if selected {
        egui::Color32::from_rgb(23, 42, 66)
    } else {
        egui::Color32::from_rgb(33, 36, 43)
    };
    let mut frame = egui::Frame::none()
        .fill(fill)
        .inner_margin(egui::Margin::same(10.0));
    if selected {
        frame = frame.stroke(egui::Stroke::new(1.5, ACCENT));
    }
    frame.show(ui, |ui| {
        ui.set_width(228.0);
        ui.set_min_height(122.0);
        ui.vertical(|ui| add(ui));
    });
    ui.add_space(2.0);
}

fn short_title(t: &str) -> String {
    if let (Some(i), Some(j)) = (t.find(" ("), t.find(')')) {
        if j > i {
            return format!("{}{}", &t[..i], &t[j + 1..]);
        }
    }
    t.to_string()
}

impl App {

    fn sel_bar(&mut self, ui: &mut egui::Ui, sugg_cat: Option<usize>) {
        let (count, bytes, paths, ctx): (usize, u64, Vec<PathBuf>, DeleteCtx) = match sugg_cat {
            Some(cat) => {
                let in_cat: Vec<PathBuf> = match self.suggestions.get(cat) {
                    Some(sg) => sg
                        .entries
                        .iter()
                        .map(|e| e.path.clone())
                        .filter(|p| self.sugg_sel.contains_key(p))
                        .collect(),
                    None => Vec::new(),
                };
                let b: u64 = in_cat
                    .iter()
                    .map(|p| self.sugg_sel.get(p).copied().unwrap_or(0))
                    .sum();
                let n = in_cat.len();
                (n, b, in_cat, DeleteCtx::Suggest(cat))
            }
            None => (
                self.search_sel.len(),
                self.search_sel.values().sum(),
                self.search_sel.keys().cloned().collect(),
                DeleteCtx::Single,
            ),
        };
        if count == 0 {
            return;
        }
        let mut act: Option<DeleteMode> = None;
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "\u{2611} {} {} selected ({})",
                    fmt_num(count),
                    if count == 1 { "file" } else { "files" },
                    fmt_size(bytes)
                ))
                .strong(),
            );
            if ui
                .add_enabled(
                    self.delete_progress.is_none(),
                    egui::Button::new("\u{1f5d1} Move to Recycle Bin"),
                )
                .clicked()
            {
                act = Some(DeleteMode::Trash);
            }
            if ui
                .add_enabled(
                    self.delete_progress.is_none(),
                    egui::Button::new("\u{26a1} Delete permanently"),
                )
                .clicked()
            {
                act = Some(DeleteMode::Permanent);
            }
            if ui.button("Clear selection").clicked() {
                clear = true;
            }
        });
        ui.add_space(4.0);
        if clear {
            match sugg_cat {
                Some(_) => self.sugg_sel.clear(),
                None => self.search_sel.clear(),
            }
            return;
        }
        if let Some(mode) = act {
            let label = match &ctx {
                DeleteCtx::Suggest(cat) => self
                    .suggestions
                    .get(*cat)
                    .map(|sg| sg.title.clone())
                    .unwrap_or_else(|| "selected files".to_string()),
                _ => "selected files".to_string(),
            };
            if mode == DeleteMode::Permanent && self.skip_perm_confirm {
                self.start_delete(paths, label, ctx, mode);
            } else {
                self.bulk_confirm = Some((paths, bytes, label, ctx, mode));
            }
        }
    }

    fn ui_bulk_confirm(&mut self, ui: &mut egui::Ui) {
        let (count, bytes, mode) = match &self.bulk_confirm {
            Some((paths, bytes, _, _, mode)) => (paths.len(), *bytes, *mode),
            None => return,
        };
        let noun = if count == 1 { "file" } else { "files" };
        let mut go = false;
        let mut cancel = false;
        egui::Frame::none()
            .fill(if mode == DeleteMode::Permanent {
                egui::Color32::from_rgb(52, 40, 40)
            } else {
                egui::Color32::from_rgb(35, 45, 60)
            })
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if mode == DeleteMode::Permanent {
                        ui.label(
                            egui::RichText::new(format!(
                                "PERMANENTLY delete {} {} ({}) \u{2014} this cannot be undone!",
                                fmt_num(count),
                                noun,
                                fmt_size(bytes)
                            ))
                            .color(egui::Color32::from_rgb(240, 110, 110))
                            .strong(),
                        );
                    } else {
                        ui.label(format!(
                            "Move {} {} ({}) to the Recycle Bin?",
                            fmt_num(count),
                            noun,
                            fmt_size(bytes)
                        ));
                    }
                    if ui
                        .add_enabled(
                            self.delete_progress.is_none(),
                            egui::Button::new("\u{2714} Delete"),
                        )
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if mode == DeleteMode::Permanent {
                    ui.checkbox(
                        &mut self.dont_ask_perm,
                        "Don't ask again \u{2014} always delete permanently without confirming",
                    );
                }
            });
        ui.add_space(6.0);
        if cancel {
            self.bulk_confirm = None;
            self.dont_ask_perm = false;
        }
        if go {
            if mode == DeleteMode::Permanent && self.dont_ask_perm {
                self.skip_perm_confirm = true;
                save_setting("skip_perm_confirm", "1");
            }
            self.dont_ask_perm = false;
            if let Some((paths, _b, label, ctx, mode)) = self.bulk_confirm.take() {
                self.start_delete(paths, label, ctx, mode);
            }
        }
    }

    fn ui_single_confirm(&mut self, ui: &mut egui::Ui) {
        let Some((path, mode)) = self.single_confirm.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut go = false;
        let mut cancel = false;
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(52, 40, 40))
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if mode == DeleteMode::Permanent {
                        ui.label(
                            egui::RichText::new(format!("PERMANENTLY delete file '{name}'?"))
                                .color(egui::Color32::from_rgb(240, 110, 110))
                                .strong(),
                        );
                    } else {
                        ui.label(format!("Move file '{name}' to the Recycle Bin?"));
                    }
                    if ui
                        .add_enabled(
                            self.delete_progress.is_none(),
                            egui::Button::new(if mode == DeleteMode::Permanent {
                                "✔ Delete it"
                            } else {
                                "✔ Delete"
                            }),
                        )
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if mode == DeleteMode::Permanent {
                    ui.checkbox(
                        &mut self.dont_ask_perm,
                        "Don't ask again \u{2014} always delete permanently without confirming",
                    );
                }
                ui.label(
                    egui::RichText::new(path.display().to_string())
                        .small()
                        .color(MUTED),
                );
            });
        ui.add_space(6.0);
        if cancel {
            self.single_confirm = None;
            self.dont_ask_perm = false;
        }
        if go {
            if mode == DeleteMode::Permanent && self.dont_ask_perm {
                self.skip_perm_confirm = true;
                save_setting("skip_perm_confirm", "1");
            }
            self.dont_ask_perm = false;
            self.single_confirm = None;
            self.start_delete(vec![path], name, DeleteCtx::Single, mode);
        }
    }

    fn ui_search(&mut self, ui: &mut egui::Ui) -> Option<FileAction> {
        if self.index.is_none() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("No folder scanned yet");
                ui.label("Pick a root folder in the sidebar and press Scan.");
            });
            return None;
        }
        ui.horizontal(|ui| {
            ui.label("🔎");
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(380.0)
                    .hint_text("Type a file name, e.g. jojo pdf  or  .cbz"),
            );
            if ui.button("Clear filter").clicked() {
                self.query.clear();
            }
            ui.label(
                egui::RichText::new(format!("{} results", self.cache.search.len()))
                    .color(MUTED),
            );
        });
        ui.add_space(4.0);
        self.sel_bar(ui, None);
        let rows = self.cache.search.clone();
        let (action, _) = ui.push_id("search-table", |ui| {
            file_table(ui, &rows, &mut self.search_sort, None, &mut self.search_sel)
        }).inner;
        action
    }

    fn ui_dupes(&mut self, ui: &mut egui::Ui) {
        if self.index.is_none() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("No folder scanned yet");
                ui.label("Scan first, then find duplicates.");
            });
            return;
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.busy.is_none(), egui::Button::new("🔍 Find duplicates"))
                .clicked()
            {
                self.start_dupes();
            }
            ui.separator();
            ui.label("Sort groups by:");
            let mut changed = false;
            for (s, label) in [
                (DSort::Wasted, "Most wasted"),
                (DSort::Size, "Largest file"),
                (DSort::Count, "Most copies"),
            ] {
                if ui.selectable_value(&mut self.dupe_sort, s, label).changed() {
                    changed = true;
                }
            }
            if changed {
                self.sort_dupes();
            }
            if self.dupes_stale && !self.dupes.is_empty() {
                ui.label(egui::RichText::new("(stale results)").color(MUTED));
            }
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.dupe_skip_protected, "Skip system/app zones")
                .on_hover_text("Does not hash files in Windows, Program Files, AppData, Pinokio/PionLogin stores, conda, Sandbox — they would not be deleted anyway, skipping is faster. Untick to include everything.");
            ui.checkbox(&mut self.dupe_skip_small, "Skip files < 4 KB")
                .on_hover_text("Duplicates of a few KB waste negligible space but make up most of the files that must be opened and read.");
        });
        if !self.dupes.is_empty() {
            let total_groups = self.dupes.len();
            let safe_groups = self
                .dupes
                .iter()
                .filter(|g| filesweep::group_all_user(&g.paths) && !g.hardlinked && !filesweep::group_has_pinokio(&g.paths))
                .count();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.busy.is_none() && self.delete_progress.is_none(),
                        egui::Button::new("⚡ Safe quick-select"),
                    )
                    .on_hover_text("Auto-tick the extra copies — ONLY in groups where every copy is outside system/app zones, is not a hardlink and does not involve Pinokio. Each group keeps the copy with the shortest path.")
                    .clicked()
                {
                    let mut ticked = 0usize;
                    let mut freed = 0u64;
                    for g in &self.dupes {
                        if filesweep::group_all_user(&g.paths) && !g.hardlinked && !filesweep::group_has_pinokio(&g.paths) {
                            let keeper = g
                                .paths
                                .iter()
                                .min_by(|a, b| {
                                    a.as_os_str()
                                        .len()
                                        .cmp(&b.as_os_str().len())
                                        .then_with(|| a.cmp(b))
                                })
                                .unwrap();
                            for p in &g.paths {
                                if p != keeper && self.selected.insert(p.clone()) {
                                    ticked += 1;
                                    freed += g.size;
                                }
                            }
                        }
                    }
                    self.confirm_delete = false;
                    self.dupes_perm_arm = false;
                    self.status = if ticked == 0 {
                        let n_pin = self
                            .dupes
                            .iter()
                            .filter(|g| !g.hardlinked && filesweep::group_has_pinokio(&g.paths))
                            .count();
                        let n_hl =
                            self.dupes.iter().filter(|g| g.hardlinked).count();
                        let n_sys = total_groups
                            .saturating_sub(safe_groups + n_pin + n_hl);
                        format!(
                            "Safe quick-select: ticked NOTHING — by design, not a bug. There are currently no plain user groups to auto-tick; the rest is {} Pinokio/app-owned groups, {} system/app groups, {} hardlink groups. App-owned files are never bulk-ticked: open each group, pick the keeper with the ★ button, then use the ⚡ button in that group.",
                            fmt_num(n_pin),
                            fmt_num(n_sys),
                            fmt_num(n_hl)
                        )
                    } else {
                        format!(
                            "Safe quick-select: ticked {} files (≈ {}) in {} user-zone groups — keeping 1 copy per group (shortest path). Skipped {} system/app, hardlink or Pinokio groups. Review before deleting.",
                            fmt_num(ticked),
                            fmt_size(freed),
                            fmt_num(safe_groups),
                            fmt_num(total_groups - safe_groups)
                        )
                    };
                }
                let mut hide = self.hide_protected;
                if ui
                    .checkbox(&mut hide, "Hide system/app-zone groups")
                    .changed()
                {
                    self.hide_protected = hide;
                }
                let hl_groups = self.dupes.iter().filter(|g| g.hardlinked).count();
                let pinokio_groups = self
                    .dupes
                    .iter()
                    .filter(|g| !g.hardlinked && filesweep::group_has_pinokio(&g.paths))
                    .count();
                let protected_groups = self
                    .dupes
                    .iter()
                    .filter(|g| {
                        !g.hardlinked
                            && !filesweep::group_has_pinokio(&g.paths)
                            && !filesweep::group_all_user(&g.paths)
                    })
                    .count();
                let user_groups = total_groups
                    .saturating_sub(hl_groups + pinokio_groups + protected_groups);
                ui.label(
                    egui::RichText::new(format!(
                        "{} user-zone groups • {} system/app groups • {} Pinokio groups • {} hardlink groups (no extra space used)",
                        fmt_num(user_groups),
                        fmt_num(protected_groups),
                        fmt_num(pinokio_groups),
                        fmt_num(hl_groups)
                    ))
                    .color(MUTED),
                );
            });
        }
        if !self.selected.is_empty() {
            ui.separator();
            if self.confirm_delete {

                let mut shown = 0usize;
                let mut affected = 0usize;
                egui::Frame::none()
                    .fill(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Verify before deleting — group by group:")
                                .strong(),
                        );
                        for g in &self.dupes {
                            let dels: Vec<&PathBuf> = g
                                .paths
                                .iter()
                                .filter(|p| self.selected.contains(*p))
                                .collect();
                            if dels.is_empty() {
                                continue;
                            }
                            affected += 1;
                            if shown >= 8 {
                                continue;
                            }
                            shown += 1;
                            let keeper = if let Some(k) =
                                self.keeper_choice.get(&(g.size, g.hash.clone()))
                            {
                                if g.paths.contains(k) {
                                    k.clone()
                                } else {
                                    g.paths
                                        .iter()
                                        .min_by(|a, b| {
                                            a.as_os_str()
                                                .len()
                                                .cmp(&b.as_os_str().len())
                                                .then_with(|| a.cmp(b))
                                        })
                                        .unwrap()
                                        .clone()
                                }
                            } else {
                                g.paths
                                    .iter()
                                    .min_by(|a, b| {
                                        a.as_os_str()
                                            .len()
                                            .cmp(&b.as_os_str().len())
                                            .then_with(|| a.cmp(b))
                                    })
                                    .unwrap()
                                    .clone()
                            };
                            if self.selected.contains(&keeper) {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "⚠ Group {}: you ticked the KEEPER itself — double-check!",
                                        fmt_size(g.size)
                                    ))
                                    .color(egui::Color32::from_rgb(240, 110, 110)),
                                );
                            } else {
                                ui.label(format!("🛡 Keep: {}", keeper.display()));
                            }
                            for d in &dels {
                                ui.label(format!("🗑 Delete: {}", d.display()));
                            }
                        }
                        if affected > shown {
                            ui.label(format!("… and {} more groups.", affected - shown));
                        }
                    });
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let sel_bytes: u64 = self
                    .dupes
                    .iter()
                    .map(|g| {
                        g.size
                            * g.paths.iter().filter(|p| self.selected.contains(*p)).count()
                                as u64
                    })
                    .sum();
                ui.label(format!(
                    "{} files selected (≈ {}) for deletion:",
                    fmt_num(self.selected.len()),
                    fmt_size(sel_bytes)
                ));
                if !self.confirm_delete {
                    if ui
                        .add_enabled(
                            self.delete_progress.is_none(),
                            egui::Button::new("🗑 Move to Recycle Bin"),
                        )
                        .clicked()
                    {
                        self.confirm_delete = true;
                        self.dupes_perm_arm = false;
                    }
                    if ui
                        .add_enabled(
                            self.delete_progress.is_none(),
                            egui::Button::new("⚡ Delete permanently (fast)"),
                        )
                        .clicked()
                    {
                        if self.skip_perm_confirm {
                            self.delete_selected_mode(DeleteMode::Permanent);
                        } else {
                            self.confirm_delete = true;
                            self.dupes_perm_arm = true;
                        }
                    }
                } else if !self.dupes_perm_arm {
                    ui.label(format!(
                        "Move {} files to the Recycle Bin?",
                        fmt_num(self.selected.len())
                    ));
                    if ui.button("✔ Yes, to the Recycle Bin").clicked() {
                        self.delete_selected_mode(DeleteMode::Trash);
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete = false;
                    }
                } else {
                    ui.label(
                        egui::RichText::new("PERMANENT DELETE — this cannot be undone!")
                            .color(egui::Color32::from_rgb(240, 110, 110))
                            .strong(),
                    );
                    ui.checkbox(&mut self.dont_ask_perm, "Don't ask again");
                    if ui.button("✔ Yes, delete permanently").clicked() {
                        if self.dont_ask_perm {
                            self.skip_perm_confirm = true;
                            save_setting("skip_perm_confirm", "1");
                            self.dont_ask_perm = false;
                        }
                        self.delete_selected_mode(DeleteMode::Permanent);
                    }
                    if ui.button("Back").clicked() {
                        self.dupes_perm_arm = false;
                    }
                }
                if ui.button("Clear selection").clicked() {
                    self.selected.clear();
                    self.confirm_delete = false;
                    self.dupes_perm_arm = false;
                }
            });
        }
        ui.separator();
        let groups: Vec<(u64, usize, u64, Vec<PathBuf>, bool, bool, String)> = self
            .dupes
            .iter()
            .map(|g| {
                (
                    g.size,
                    g.paths.len(),
                    g.wasted(),
                    g.paths.clone(),
                    filesweep::group_all_user(&g.paths),
                    g.hardlinked,
                    g.hash.clone(),
                )
            })
            .collect();
        let total_wasted: u64 = groups.iter().map(|g| g.2).sum();
        let visible: Vec<&(u64, usize, u64, Vec<PathBuf>, bool, bool, String)> = groups
            .iter()
            .filter(|g| !self.hide_protected || g.4)
            .collect();
        if !groups.is_empty() {
            ui.label(format!(
                "{} duplicate groups — total wasted {}{}",
                groups.len(),
                fmt_size(total_wasted),
                if self.hide_protected && visible.len() < groups.len() {
                    format!(" (hiding {} system/app groups)", groups.len() - visible.len())
                } else {
                    String::new()
                }
            ));
            ui.add_space(2.0);
        }

        let keeper_of = |size: u64, hash: &str, paths: &[PathBuf]| -> PathBuf {
            if let Some(k) = self.keeper_choice.get(&(size, hash.to_string())) {
                if paths.contains(k) {
                    return k.clone();
                }
            }
            paths
                .iter()
                .min_by(|a, b| {
                    a.as_os_str()
                        .len()
                        .cmp(&b.as_os_str().len())
                        .then_with(|| a.cmp(b))
                })
                .unwrap()
                .clone()
        };
        let mut group_quick: Option<(Vec<PathBuf>, PathBuf, usize)> = None;
        let mut keep_pick: Option<((u64, String), PathBuf, usize)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (gi, (size, count, wasted, paths, all_user, hardlinked, hash)) in visible.iter().enumerate() {
                    let mut title = format!(
                        "Group {}: {} × {} copies — {} wasted",
                        gi + 1,
                        fmt_size(*size),
                        count,
                        fmt_size(*wasted)
                    );
                    if *hardlinked {
                        title += " — 🔗 hardlink, uses no real extra space";
                    }
                    if !all_user {
                        title += " — ⚠ system/app zone";
                    }
                    let mut rt = egui::RichText::new(title).strong();
                    if !all_user {
                        rt = rt.color(egui::Color32::from_rgb(240, 190, 90));
                    }
                    egui::CollapsingHeader::new(rt)
                        .id_salt(gi)
                        .default_open(gi < 5)
                        .show(ui, |ui| {
                            if *hardlinked {
                                ui.label(
                                    egui::RichText::new("These paths all point to a single physical copy of the data (hardlink) — deleting one name frees no space, so this group is excluded from the wasted total and Safe quick-select skips it.")
                                        .color(egui::Color32::from_rgb(140, 200, 255)),
                                );
                            }
                            if !all_user {
                                ui.label(
                                    egui::RichText::new("This group has copies in system/app zones (Windows, Program Files, Pinokio store, conda, Sandbox...) — quick-select skips it. Tick manually only if you really know what you are doing.")
                                        .color(egui::Color32::from_rgb(240, 190, 90)),
                                );
                            }
                            if !*hardlinked {
                                let keeper = keeper_of(*size, hash, paths);
                                let user_chosen = self
                                    .keeper_choice
                                    .get(&(*size, hash.clone()))
                                    .map(|k| paths.contains(k))
                                    .unwrap_or(false);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "🛡 Keeper: {}{}",
                                        keeper.display(),
                                        if user_chosen { " (your pick)" } else { "" }
                                    ))
                                    .color(egui::Color32::from_rgb(130, 215, 165)),
                                );
                                let keeper_name = keeper
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                if ui
                                    .button(format!(
                                        "⚡ Tick extra copies in this group (keeping: {})",
                                        keeper_name
                                    ))
                                    .on_hover_text(format!(
                                        "Keeper: {}\nTick every other copy in this group — this group only. You still press delete yourself afterwards.",
                                        keeper.display()
                                    ))
                                    .clicked()
                                {
                                    group_quick = Some((paths.clone(), keeper, gi + 1));
                                }
                            }
                            for p in paths {
                                ui.horizontal(|ui| {
                                    let mut checked = self.selected.contains(p);
                                    let resp =
                                        ui.checkbox(&mut checked, p.display().to_string());
                                    if resp.changed() {
                                        if checked {
                                            self.selected.insert(p.clone());
                                        } else {
                                            self.selected.remove(p);
                                        }
                                        self.confirm_delete = false;
                                    }
                                    if ui.small_button("↗ Open").clicked() {
                                        if let Some(parent) = p.parent() {
                                            let _ = open::that(parent);
                                        }
                                    }
                                    if !*hardlinked
                                        && ui
                                            .small_button("★ Keep")
                                            .on_hover_text("Keep this copy — treat the rest of the group as duplicates")
                                            .clicked()
                                    {
                                        keep_pick =
                                            Some(((*size, hash.clone()), p.clone(), gi + 1));
                                    }
                                });
                            }
                        });
                }
            });
        if let Some((key, path, gno)) = keep_pick {
            self.keeper_choice.insert(key, path.clone());
            self.status = format!(
                "Set {} as the keeper of group {} — press the ⚡ button in that group to tick the other copies.",
                path.display(),
                gno
            );
        }
        if let Some((paths, keeper, gno)) = group_quick {
            let mut n = 0usize;
            for p in &paths {
                if *p != keeper && self.selected.insert(p.clone()) {
                    n += 1;
                }
            }
            self.confirm_delete = false;
            self.dupes_perm_arm = false;
            self.status = format!(
                "Ticked {} extra copies in group {} — keeping: {}. Review before deleting.",
                fmt_num(n),
                gno,
                keeper.display()
            );
        }
    }

    fn ui_suggest(&mut self, ui: &mut egui::Ui) {
        if self.index.is_none() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("No folder scanned yet");
                ui.label("Scan first — the app analyzes and suggests what is safe to delete.");
            });
            return;
        }
        let safe_total: u64 = self
            .suggestions
            .iter()
            .filter(|s| s.tier == filesweep::Tier::Safe && s.deletable)
            .map(|s| s.total())
            .sum();
        let recycle_total: u64 = self
            .suggestions
            .iter()
            .filter(|s| s.tier == filesweep::Tier::Safe && !s.deletable)
            .map(|s| s.total())
            .sum();
        let dupe_wasted: u64 = if self.dupes_stale {
            0
        } else {
            self.dupes.iter().map(|g| g.wasted()).sum()
        };
        let empty_count = self.index.as_ref().map(|i| i.empty_dirs.len()).unwrap_or(0);

        egui::Frame::none()
            .fill(egui::Color32::from_rgb(24, 33, 26))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Safe to clean right now: {}",
                        fmt_size(safe_total + dupe_wasted)
                    ))
                    .heading()
                    .color(egui::Color32::from_rgb(120, 220, 140)),
                );
                let mut parts: Vec<String> = Vec::new();
                if safe_total > 0 {
                    parts.push(format!("Safe cards: {}", fmt_size(safe_total)));
                }
                if dupe_wasted > 0 {
                    parts.push(format!("duplicates: {}", fmt_size(dupe_wasted)));
                }
                if recycle_total > 0 {
                    parts.push(format!("plus {} more if you empty the Recycle Bin yourself", fmt_size(recycle_total)));
                }
                if !parts.is_empty() {
                    ui.label(egui::RichText::new(parts.join("  •  ")).color(MUTED));
                }
                ui.label("Click a card to see details and delete in the panel below. Deleting here always goes to the Recycle Bin — restorable. Files owned by their own apps (e.g. the Pinokio vault store) are NEVER classified as safe to delete.");
            });
        ui.add_space(8.0);
        if self.analyze_busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Analyzing cleanable items...");
            });
        }

        let mut select_card: Option<usize> = None;
        let mut compute_dupes = false;
        let mut open_dupes_tab = false;
        let mut open_disk_tab = false;
        egui::ScrollArea::horizontal()
            .id_salt("suggest-cards")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {

                    card_frame(ui, false, |ui| {
                        ui.label(egui::RichText::new("👯 Duplicates").strong());
                        if self.dupes_stale {
                            ui.label(
                                egui::RichText::new("Not computed for this scan yet")
                                    .color(MUTED),
                            );
                            ui.add_space(14.0);
                            if ui.button("Find duplicates").clicked() {
                                compute_dupes = true;
                            }
                        } else {
                            ui.label(
                                egui::RichText::new(fmt_size(dupe_wasted))
                                    .heading()
                                    .color(ACCENT),
                            );
                            ui.label(
                                egui::RichText::new(format!("{} duplicate groups", fmt_num(self.dupes.len())))
                                    .color(MUTED),
                            );
                            if ui.button("Open the Duplicates tab").clicked() {
                                open_dupes_tab = true;
                            }
                        }
                    });

                    for (i, s) in self.suggestions.iter().enumerate() {
                        let selected = self.selected_suggest == i;
                        let (tier_text, tier_color) = match s.tier {
                            filesweep::Tier::Safe => ("✅ SAFE", egui::Color32::from_rgb(120, 220, 140)),
                            filesweep::Tier::Caution => ("⚠️ CAUTION", egui::Color32::from_rgb(240, 190, 90)),
                        };
                        card_frame(ui, selected, |ui| {
                            ui.label(egui::RichText::new(tier_text).color(tier_color).strong());
                            ui.label(egui::RichText::new(short_title(&s.title)).strong())
                                .on_hover_text(s.title.clone());
                            ui.label(
                                egui::RichText::new(fmt_size(s.total())).heading().color(ACCENT),
                            );
                            ui.label(
                                egui::RichText::new(format!("{} file", fmt_num(s.entries.len())))
                                    .color(MUTED),
                            );
                            if ui
                                .button(if selected { "Viewing ▾" } else { "View details" })
                                .clicked()
                            {
                                select_card = Some(i);
                            }
                        });
                    }

                    card_frame(ui, false, |ui| {
                        ui.label(egui::RichText::new("📁 Empty folders").strong());
                        ui.label(
                            egui::RichText::new(fmt_num(empty_count)).heading().color(ACCENT),
                        );
                        ui.label(egui::RichText::new("folder contains no files").color(MUTED));
                        ui.add_space(2.0);
                        if ui.button("View in the Disk tab").clicked() {
                            open_disk_tab = true;
                        }
                    });
                });
            });
        if let Some(i) = select_card {
            self.selected_suggest = i;
            self.suggest_confirm = None;
            self.suggest_perm_arm = false;
        }
        if compute_dupes {
            self.start_dupes();
        }
        if open_dupes_tab {
            self.tab = Tab::Dupes;
        }
        if open_disk_tab {
            self.tab = Tab::Disk;
        }

        ui.add_space(8.0);
        if self.suggestions.is_empty() {
            if !self.analyze_busy {
                ui.label("No suggestions for this scan yet.");
            }
            return;
        }
        let sel = self.selected_suggest.min(self.suggestions.len() - 1);
        self.selected_suggest = sel;
        let mut delete_mode: Option<DeleteMode> = None;
        let mut open_path: Option<PathBuf> = None;
        let mut single_act: Option<(PathBuf, DeleteMode)> = None;
        self.sel_bar(ui, Some(sel));
        {
            let s = &self.suggestions[sel];
            ui.label(egui::RichText::new(s.title.clone()).heading());
            ui.label(s.advice.clone());
            if s.tier == filesweep::Tier::Caution {
                ui.label(
                    egui::RichText::new("Caution category: the app only lists these, it never deletes them — check each file yourself.")
                        .color(egui::Color32::from_rgb(240, 190, 90)),
                );
            }
            if s.deletable && !s.entries.is_empty() {
                ui.add_space(2.0);
                if self.suggest_confirm.is_none() {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.delete_progress.is_none(),
                                egui::Button::new("🗑 Move whole category to Recycle Bin"),
                            )
                            .clicked()
                        {
                            self.suggest_confirm = Some(sel);
                            self.suggest_perm_arm = false;
                        }
                        if ui
                            .add_enabled(
                                self.delete_progress.is_none(),
                                egui::Button::new("⚡ Delete whole category permanently (fast)"),
                            )
                            .on_hover_text("Delete directly like CCleaner — much faster but NOT recoverable")
                            .clicked()
                        {
                            if self.skip_perm_confirm {
                                delete_mode = Some(DeleteMode::Permanent);
                            } else {
                                self.suggest_confirm = Some(sel);
                                self.suggest_perm_arm = true;
                            }
                        }
                    });
                } else if self.suggest_confirm == Some(sel) && !self.suggest_perm_arm {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Move all {} files ({}) to the Recycle Bin?",
                            fmt_num(s.entries.len()),
                            fmt_size(s.total())
                        ));
                        if ui.button("✔ Yes, to the Recycle Bin").clicked() {
                            delete_mode = Some(DeleteMode::Trash);
                        }
                        if ui.button("Cancel").clicked() {
                            self.suggest_confirm = None;
                        }
                    });
                } else if self.suggest_confirm == Some(sel) && self.suggest_perm_arm {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "PERMANENTLY delete {} files ({}) — this cannot be undone!",
                                fmt_num(s.entries.len()),
                                fmt_size(s.total())
                            ))
                            .color(egui::Color32::from_rgb(240, 110, 110))
                            .strong(),
                        );
                        ui.checkbox(&mut self.dont_ask_perm, "Don't ask again");
                        if ui.button("✔ Yes, delete permanently").clicked() {
                            if self.dont_ask_perm {
                                self.skip_perm_confirm = true;
                                save_setting("skip_perm_confirm", "1");
                                self.dont_ask_perm = false;
                            }
                            delete_mode = Some(DeleteMode::Permanent);
                        }
                        if ui.button("Back").clicked() {
                            self.suggest_perm_arm = false;
                        }
                    });
                }
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("suggest-detail")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for e in s.entries.iter().take(300) {
                        ui.horizontal(|ui| {
                            let mut on = self.sugg_sel.contains_key(&e.path);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    self.sugg_sel.insert(e.path.clone(), e.size);
                                } else {
                                    self.sugg_sel.remove(&e.path);
                                }
                            }
                            ui.label(egui::RichText::new(fmt_size(e.size)).color(ACCENT));
                            ui.label(fmt_date(e.modified_secs));
                            if ui.small_button("↗ Open").clicked() {
                                if let Some(parent) = e.path.parent() {
                                    open_path = Some(parent.to_path_buf());
                                }
                            }
                            if ui
                                .small_button("🗑")
                                .on_hover_text("Move this file to the Recycle Bin")
                                .clicked()
                            {
                                single_act = Some((e.path.clone(), DeleteMode::Trash));
                            }
                            if ui
                                .small_button("⚡")
                                .on_hover_text("Permanently delete this file (fast, not recoverable)")
                                .clicked()
                            {
                                single_act = Some((e.path.clone(), DeleteMode::Permanent));
                            }
                            ui.label(
                                egui::RichText::new(e.path.display().to_string()).color(MUTED),
                            )
                            .on_hover_text(e.path.display().to_string());
                        });
                    }
                    if s.entries.len() > 300 {
                        ui.label(format!(
                            "... and {} more files (deleting the whole category includes all of them)",
                            fmt_num(s.entries.len() - 300)
                        ));
                    }
                });
        }
        if let Some(p) = open_path {
            let _ = open::that(&p);
        }
        if let Some(act) = single_act {
            if act.1 == DeleteMode::Permanent && self.skip_perm_confirm {
                let name = act
                    .0
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| act.0.display().to_string());
                self.start_delete(vec![act.0.clone()], name, DeleteCtx::Single, DeleteMode::Permanent);
            } else {
                self.single_confirm = Some(act);
            }
        }
        if let Some(mode) = delete_mode {
            self.suggest_confirm = None;
            self.suggest_perm_arm = false;
            let s = &self.suggestions[sel];
            let paths: Vec<PathBuf> = s.entries.iter().map(|e| e.path.clone()).collect();
            let label = s.title.clone();
            self.start_delete(paths, label, DeleteCtx::Suggest(sel), mode);
        }
    }

    fn ui_disk(&mut self, ui: &mut egui::Ui) -> Option<FileAction> {
        if self.index.is_none() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("No folder scanned yet");
                ui.label("Run a Scan first.");
            });
            return None;
        }
        let mut action: Option<FileAction> = None;
        ui.label(egui::RichText::new("Size by top-level subfolder").heading());

        let toplevel = self.cache.toplevel.clone();
        let mut tsort_changed = false;
        let mut tsort = self.toplevel_sort;
        ui.push_id("toplevel-table", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder().clip(true).resizable(false))
                .column(Column::initial(110.0).range(80.0..=180.0))
                .column(Column::initial(90.0).range(70.0..=140.0))
                .column(Column::initial(128.0).range(110.0..=150.0))
                .max_scroll_height(210.0)
                .header(24.0, |mut header| {
                for (col, label) in [
                    (TCol::Dir, "Folder"),
                    (TCol::Size, "Size"),
                    (TCol::Count, "Files"),
                ] {
                    header.col(|ui| {
                        let active = tsort.0 == col;
                        let text = if active {
                            format!("{}{}", label, sort_arrow(tsort.1))
                        } else {
                            label.to_string()
                        };
                        let mut rt = egui::RichText::new(text).strong();
                        if active {
                            rt = rt.color(ACCENT);
                        }
                        if ui.button(rt).clicked() {
                            if active {
                                tsort.1 = !tsort.1;
                            } else {
                                tsort = (col, col == TCol::Dir);
                            }
                            tsort_changed = true;
                        }
                    });
                }
                header.col(|ui| {
                    ui.label(egui::RichText::new("Actions").strong());
                });
            })
            .body(|body| {
                body.rows(22.0, toplevel.len(), |mut row| {
                    let (dir, bytes, count) = &toplevel[row.index()];
                    row.col(|ui| {
                        ui.label(dir.display().to_string())
                            .on_hover_text(dir.display().to_string());
                    });
                    row.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(fmt_size(*bytes));
                        });
                    });
                    row.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(count.to_string());
                        });
                    });
                    row.col(|ui| {
                        if ui.small_button("Open").clicked() {
                            action = Some(FileAction::Navigate(dir.clone()));
                        }
                        if ui.small_button("↗ Open").clicked() {
                            action = Some(FileAction::OpenExplorer(dir.clone()));
                        }
                    });
                });
            });
        });
        if tsort_changed {
            self.toplevel_sort = tsort;
        }

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Largest files").heading());
        let largest = self.cache.largest.clone();
        let (a2, _) = ui.push_id("largest-table", |ui| {
            file_table(ui, &largest, &mut self.largest_sort, Some(240.0), &mut self.search_sel)
        }).inner;
        if a2.is_some() {
            action = a2;
        }
        self.sel_bar(ui, None);

        ui.add_space(6.0);
        if let Some(idx) = &self.index {
            ui.label(
                egui::RichText::new(format!("Empty folders: {}", idx.empty_dirs.len()))
                    .heading(),
            );
            ui.label(
                egui::RichText::new("Contains no files — delete manually in Explorer if you are sure it is not needed.")
                    .color(MUTED),
            );
            egui::ScrollArea::vertical()
                .id_salt("empty-dirs")
                .max_height(170.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for d in idx.empty_dirs.iter().take(300) {
                        ui.horizontal(|ui| {
                            if ui.small_button("↗ Open").clicked() {
                                let _ = open::that(d);
                            }
                            ui.label(d.display().to_string());
                        });
                    }
                    if idx.empty_dirs.len() > 300 {
                        ui.label(format!("... and {} more folders", idx.empty_dirs.len() - 300));
                    }
                });
        }
        action
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([860.0, 540.0])
            .with_title(concat!("FileSweep v", env!("CARGO_PKG_VERSION"), " — Fast File Search · Duplicate Finder · Disk Cleanup")),
        ..Default::default()
    };
    eframe::run_native("FileSweep", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
