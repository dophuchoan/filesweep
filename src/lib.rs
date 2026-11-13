use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub name_lower: String,
    pub path_lower: String,
    pub size: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub root: PathBuf,
    pub entries: Vec<FileEntry>,
    pub total_bytes: u64,
    pub empty_dirs: Vec<PathBuf>,
    pub scan_ms: u128,
}

#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub size: u64,
    pub hash: String,
    pub paths: Vec<PathBuf>,

    pub hardlinked: bool,

    pub distinct_files: usize,
}

impl DupeGroup {

    pub fn wasted(&self) -> u64 {
        if self.distinct_files <= 1 {
            0
        } else {
            self.size * (self.distinct_files as u64 - 1)
        }
    }
}

pub fn scan(root: &Path) -> Index {
    let t0 = Instant::now();
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut all_dirs: Vec<PathBuf> = Vec::new();
    let mut dirs_with_files: HashSet<PathBuf> = HashSet::new();
    let mut total_bytes: u64 = 0;

    for e in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        let path = e.path().to_path_buf();
        if e.file_type().is_dir() {
            all_dirs.push(path);
            continue;
        }
        if !e.file_type().is_file() {
            continue;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = md.len();
        total_bytes += size;
        if let Some(parent) = path.parent() {

            let mut cur = Some(parent.to_path_buf());
            while let Some(d) = cur {
                if !dirs_with_files.insert(d.clone()) {
                    break;
                }
                if d == root {
                    break;
                }
                cur = d.parent().map(|p| p.to_path_buf());
            }
        }
        let name = e.file_name().to_string_lossy().to_string();
        let modified_secs = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push(FileEntry {
            name_lower: name.to_lowercase(),
            path_lower: path.to_string_lossy().to_lowercase(),
            path,
            name,
            size,
            modified_secs,
        });
    }

    let mut empty_dirs: Vec<PathBuf> = all_dirs
        .into_iter()
        .filter(|d| !dirs_with_files.contains(d))
        .collect();
    empty_dirs.sort();

    Index {
        root: root.to_path_buf(),
        entries,
        total_bytes,
        empty_dirs,
        scan_ms: t0.elapsed().as_millis(),
    }
}

const INDEX_MAGIC: &[u8; 5] = b"TFIX1";

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

pub fn save_index(path: &Path, index: &Index, saved_at_secs: u64) -> std::io::Result<()> {
    let mut dir_ids: HashMap<String, u32> = HashMap::new();
    let mut dirs: Vec<String> = Vec::new();
    let mut rows: Vec<(u32, &FileEntry)> = Vec::with_capacity(index.entries.len());
    for e in &index.entries {
        let parent = e
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let id = *dir_ids.entry(parent.clone()).or_insert_with(|| {
            dirs.push(parent);
            (dirs.len() - 1) as u32
        });
        rows.push((id, e));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(index.entries.len() * 40 + dirs.len() * 60);
    buf.extend_from_slice(INDEX_MAGIC);
    put_u64(&mut buf, saved_at_secs);
    put_str(&mut buf, &index.root.to_string_lossy());
    put_u64(&mut buf, index.scan_ms as u64);
    put_u64(&mut buf, index.total_bytes);
    put_u32(&mut buf, dirs.len() as u32);
    for d in &dirs {
        put_str(&mut buf, d);
    }
    put_u32(&mut buf, rows.len() as u32);
    for (dir_id, e) in &rows {
        put_u32(&mut buf, *dir_id);
        put_str(&mut buf, &e.name);
        put_u64(&mut buf, e.size);
        put_u64(&mut buf, e.modified_secs);
    }
    put_u32(&mut buf, index.empty_dirs.len() as u32);
    for d in &index.empty_dirs {
        put_str(&mut buf, &d.to_string_lossy());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, path)
}

pub fn load_index(path: &Path) -> Option<(Index, u64)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 13 || &data[..5] != INDEX_MAGIC {
        return None;
    }
    let mut cur = 5usize;
    let rd_u32 = |d: &[u8], c: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(d.get(*c..*c + 4)?.try_into().ok()?);
        *c += 4;
        Some(v)
    };
    let rd_u64 = |d: &[u8], c: &mut usize| -> Option<u64> {
        let v = u64::from_le_bytes(d.get(*c..*c + 8)?.try_into().ok()?);
        *c += 8;
        Some(v)
    };
    let rd_str = |d: &[u8], c: &mut usize| -> Option<String> {
        let n = rd_u32(d, c)? as usize;
        let b = d.get(*c..*c + n)?;
        *c += n;
        Some(String::from_utf8_lossy(b).into_owned())
    };
    let saved_at = rd_u64(&data, &mut cur)?;
    let root = PathBuf::from(rd_str(&data, &mut cur)?);
    let scan_ms = rd_u64(&data, &mut cur)? as u128;
    let total_bytes = rd_u64(&data, &mut cur)?;
    let n_dirs = rd_u32(&data, &mut cur)? as usize;
    let mut dirs: Vec<String> = Vec::with_capacity(n_dirs.min(10_000_000));
    for _ in 0..n_dirs {
        dirs.push(rd_str(&data, &mut cur)?);
    }
    let n = rd_u32(&data, &mut cur)? as usize;
    let mut entries: Vec<FileEntry> = Vec::with_capacity(n.min(50_000_000));
    for _ in 0..n {
        let dir_id = rd_u32(&data, &mut cur)? as usize;
        let name = rd_str(&data, &mut cur)?;
        let size = rd_u64(&data, &mut cur)?;
        let modified_secs = rd_u64(&data, &mut cur)?;
        let dir = dirs.get(dir_id)?;
        let path = if dir.is_empty() {
            PathBuf::from(&name)
        } else {
            PathBuf::from(dir).join(&name)
        };
        entries.push(FileEntry {
            name_lower: name.to_lowercase(),
            path_lower: path.to_string_lossy().to_lowercase(),
            path,
            name,
            size,
            modified_secs,
        });
    }
    let n_empty = rd_u32(&data, &mut cur)? as usize;
    let mut empty_dirs: Vec<PathBuf> = Vec::with_capacity(n_empty.min(10_000_000));
    for _ in 0..n_empty {
        empty_dirs.push(PathBuf::from(rd_str(&data, &mut cur)?));
    }
    Some((
        Index {
            root,
            entries,
            total_bytes,
            empty_dirs,
            scan_ms,
        },
        saved_at,
    ))
}

pub fn search<'a>(index: &'a Index, query: &str, limit: usize) -> Vec<&'a FileEntry> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return index.entries.iter().take(limit).collect();
    }
    let tokens: Vec<&str> = q.split_whitespace().collect();
    index
        .entries
        .iter()
        .filter(|e| {
            tokens
                .iter()
                .all(|t| e.name_lower.contains(t) || e.path_lower.contains(t))
        })
        .take(limit)
        .collect()
    }

const SAMPLE_BYTES: u64 = 128 * 1024;

fn hash_sample(path: &Path, size: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let mut buf = vec![0u8; SAMPLE_BYTES as usize];
    if size <= SAMPLE_BYTES {
        let n = f.read(&mut buf)?;
        hasher.update(&buf[..n]);
    } else {

        let head = f.read(&mut buf)?;
        hasher.update(&buf[..head]);
        f.seek(SeekFrom::End(-(SAMPLE_BYTES as i64)))?;
        let mut tail = vec![0u8; SAMPLE_BYTES as usize];
        let n = f.read(&mut tail)?;
        hasher.update(&tail[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_full(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[derive(Debug, Clone)]
pub struct DupeOptions {

    pub min_size: u64,

    pub skip_protected: bool,

    pub threads: usize,
}

impl DupeOptions {

    pub fn legacy() -> Self {
        DupeOptions { min_size: 0, skip_protected: false, threads: 0 }
    }
}

impl Default for DupeOptions {
    fn default() -> Self {
        DupeOptions { min_size: 4096, skip_protected: true, threads: 0 }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DupeStats {

    pub candidates: usize,

    pub hashed: usize,

    pub cache_hits: usize,

    pub skipped: usize,

    pub secs: f64,

    pub hardlink_groups: usize,

    pub hardlink_bytes: u64,
}

pub type HashCache = HashMap<(PathBuf, u64, u64), (Option<String>, Option<String>)>;

const CACHE_MAGIC: &[u8; 5] = b"TFHC1";

pub fn load_hash_cache(path: &Path) -> HashCache {
    let mut out: HashCache = HashMap::new();
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return out,
    };
    if data.len() < 9 || &data[..5] != CACHE_MAGIC {
        return out;
    }
    let mut cur = 5usize;
    let rd_u32 = |d: &[u8], c: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(d.get(*c..*c + 4)?.try_into().ok()?);
        *c += 4;
        Some(v)
    };
    let rd_u64 = |d: &[u8], c: &mut usize| -> Option<u64> {
        let v = u64::from_le_bytes(d.get(*c..*c + 8)?.try_into().ok()?);
        *c += 8;
        Some(v)
    };
    let rd_bytes = |d: &[u8], c: &mut usize, n: usize| -> Option<Vec<u8>> {
        let v = d.get(*c..*c + n)?.to_vec();
        *c += n;
        Some(v)
    };
    let n = match rd_u32(&data, &mut cur) {
        Some(v) => v as usize,
        None => return HashMap::new(),
    };
    for _ in 0..n {
        let plen = match rd_u32(&data, &mut cur) {
            Some(v) => v as usize,
            None => return HashMap::new(),
        };
        let pbytes = match rd_bytes(&data, &mut cur, plen) {
            Some(v) => v,
            None => return HashMap::new(),
        };
        let size = rd_u64(&data, &mut cur);
        let mtime = rd_u64(&data, &mut cur);
        let flags = data.get(cur).copied();
        if flags.is_some() {
            cur += 1;
        }
        let (size, mtime, flags) = match (size, mtime, flags) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return HashMap::new(),
        };
        let mut get_hash = |c: &mut usize| -> Option<Option<String>> {
            let l = rd_u32(&data, c)? as usize;
            let b = rd_bytes(&data, c, l)?;
            Some(Some(String::from_utf8_lossy(&b).into_owned()))
        };
        let sample = if flags & 1 != 0 {
            match get_hash(&mut cur) {
                Some(v) => v,
                None => return HashMap::new(),
            }
        } else {
            None
        };
        let full = if flags & 2 != 0 {
            match get_hash(&mut cur) {
                Some(v) => v,
                None => return HashMap::new(),
            }
        } else {
            None
        };
        let p = PathBuf::from(String::from_utf8_lossy(&pbytes).into_owned());
        out.insert((p, size, mtime), (sample, full));
    }
    out
}

pub fn save_hash_cache(path: &Path, cache: &HashCache) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(cache.len() * 96);
    buf.extend_from_slice(CACHE_MAGIC);
    buf.extend_from_slice(&(cache.len() as u32).to_le_bytes());
    for ((p, size, mtime), (sample, full)) in cache {
        let ps = p.to_string_lossy();
        buf.extend_from_slice(&(ps.len() as u32).to_le_bytes());
        buf.extend_from_slice(ps.as_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&mtime.to_le_bytes());
        let mut flags: u8 = 0;
        if sample.is_some() {
            flags |= 1;
        }
        if full.is_some() {
            flags |= 2;
        }
        buf.push(flags);
        for h in [sample, full].into_iter().flatten() {
            buf.extend_from_slice(&(h.len() as u32).to_le_bytes());
            buf.extend_from_slice(h.as_bytes());
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, path)
}

fn par_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    cancel: &std::sync::atomic::AtomicBool,
    f: &(dyn Fn(&T) -> R + Sync),
) -> Vec<Option<R>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let slots: Vec<std::sync::Mutex<Option<R>>> =
        (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    let cursor = AtomicUsize::new(0);
    let tc = threads.max(1).min(n);
    std::thread::scope(|s| {
        for _ in 0..tc {
            s.spawn(|| loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(&items[i]);
                *slots[i].lock().unwrap() = Some(r);
            });
        }
    });
    slots.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

#[cfg(windows)]
fn file_link_key(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: usize,
        ) -> usize;
        fn GetFileInformationByHandle(
            file: usize,
            info: *mut ByHandleFileInformation,
        ) -> i32;
        fn CloseHandle(object: usize) -> i32;
    }
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE: usize = usize::MAX;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        );
        if h == INVALID_HANDLE || h == 0 {
            return None;
        }
        let mut info = std::mem::zeroed::<ByHandleFileInformation>();
        let ok = GetFileInformationByHandle(h, &mut info);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let index = ((info.file_index_high as u64) << 32) | info.file_index_low as u64;
        Some((info.volume_serial_number as u64, index))
    }
}

#[cfg(unix)]
fn file_link_key(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    Some((md.dev(), md.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_link_key(_path: &Path) -> Option<(u64, u64)> {
    None
}

pub fn find_duplicates_fast(
    entries: &[FileEntry],
    opts: &DupeOptions,
    cache_in: Option<HashCache>,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> (Vec<DupeGroup>, DupeStats, HashCache) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let t0 = std::time::Instant::now();
    let mut cache: HashCache = cache_in.unwrap_or_default();
    let threads = if opts.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8)
    } else {
        opts.threads
    };

    let mut by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for e in entries {
        if e.size > 0 {
            by_size.entry(e.size).or_default().push(e);
        }
    }
    let mut size_groups: Vec<(u64, Vec<&FileEntry>)> =
        by_size.into_iter().filter(|(_, v)| v.len() >= 2).collect();
    size_groups
        .sort_by(|a, b| (b.0.saturating_mul(b.1.len() as u64)).cmp(&a.0.saturating_mul(a.1.len() as u64)));

    let mut stats = DupeStats::default();
    let mut plan: Vec<(u64, Vec<&FileEntry>)> = Vec::new();
    let mut total = 0usize;
    for (size, v) in size_groups {
        if size < opts.min_size {
            stats.skipped += v.len();
            continue;
        }
        let mut kept: Vec<&FileEntry> = Vec::with_capacity(v.len());
        for e in v {
            if opts.skip_protected && path_zone(&e.path) == Zone::Protected {
                stats.skipped += 1;
            } else {
                kept.push(e);
            }
        }
        if kept.len() >= 2 {
            total += kept.len();
            plan.push((size, kept));
        } else {
            stats.skipped += kept.len();
        }
    }
    stats.candidates = total;

    let done = AtomicUsize::new(0usize);
    let hashed = AtomicUsize::new(0usize);
    let hits = AtomicUsize::new(0usize);
    let report = |d: &AtomicUsize| {
        progress(d.load(Ordering::Relaxed), total);
    };

    let mut groups: Vec<DupeGroup> = Vec::new();
    'outer: for (size, cands) in &plan {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let cache_ref = &cache;
        let sample_res = par_map(cands, threads, cancel, &|e: &&FileEntry| {
            let key = (e.path.clone(), e.size, e.modified_secs);
            if let Some((s, _)) = cache_ref.get(&key) {
                if let Some(s) = s {
                    hits.fetch_add(1, Ordering::Relaxed);
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 256 == 0 {
                        progress(d, total);
                    }
                    return (key, Some(s.clone()), false);
                }
            }
            let r = hash_sample(&e.path, *size).ok();
            if r.is_some() {
                hashed.fetch_add(1, Ordering::Relaxed);
            }
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 256 == 0 {
                progress(d, total);
            }
            (key, r, true)
        });

        let mut by_sample: HashMap<String, Vec<(PathBuf, u64)>> = HashMap::new();
        for item in sample_res.into_iter().flatten() {
            let (key, sample, computed) = item;
            if let Some(s) = sample {
                if computed {
                    let ent = cache.entry(key.clone()).or_insert((None, None));
                    if ent.0.is_none() {
                        ent.0 = Some(s.clone());
                    }
                }
                by_sample.entry(s).or_default().push((key.0, key.2));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            break 'outer;
        }

        for (_, same) in by_sample {
            if same.len() < 2 {
                continue;
            }
            if cancel.load(Ordering::Relaxed) {
                break 'outer;
            }
            let cache_ref2 = &cache;
            let full_res = par_map(&same, threads, cancel, &|kp: &(PathBuf, u64)| {
                let key = (kp.0.clone(), *size, kp.1);
                if let Some((_, f)) = cache_ref2.get(&key) {
                    if let Some(f) = f {
                        hits.fetch_add(1, Ordering::Relaxed);
                        return (key, Some(f.clone()), false);
                    }
                }
                let r = hash_full(&kp.0).ok();
                if r.is_some() {
                    hashed.fetch_add(1, Ordering::Relaxed);
                }
                (key, r, true)
            });
            let mut by_full: HashMap<String, Vec<PathBuf>> = HashMap::new();
            for item in full_res.into_iter().flatten() {
                let (key, full, computed) = item;
                if let Some(f) = full {
                    if computed {
                        let ent = cache.entry(key.clone()).or_insert((None, None));
                        if ent.1.is_none() {
                            ent.1 = Some(f.clone());
                        }
                    }
                    by_full.entry(f).or_default().push(key.0);
                }
            }
            for (hash, mut paths) in by_full {
                if paths.len() > 1 {
                    paths.sort();
                    let n = paths.len();
                    groups.push(DupeGroup { size: *size, hash, paths, hardlinked: false, distinct_files: n });
                }
            }
        }
    }
    report(&done);
    stats.hashed = hashed.load(Ordering::Relaxed);
    stats.cache_hits = hits.load(Ordering::Relaxed);
    stats.secs = t0.elapsed().as_secs_f64();

    let mut hl_groups = 0usize;
    let mut hl_bytes = 0u64;
    for g in &mut groups {
        let mut distinct: HashSet<(u64, u64)> = HashSet::new();
        let mut unknown = 0usize;
        for p in &g.paths {
            match file_link_key(p) {
                Some(k) => {
                    distinct.insert(k);
                }
                None => unknown += 1,
            }
        }
        g.distinct_files = distinct.len() + unknown;
        g.hardlinked = g.paths.len() > 1 && g.distinct_files == 1;
        if g.hardlinked {
            hl_groups += 1;
            hl_bytes += g.size * (g.paths.len() as u64 - 1);
        }
    }
    stats.hardlink_groups = hl_groups;
    stats.hardlink_bytes = hl_bytes;
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()));

    if !cancel.load(Ordering::Relaxed) {
        let alive: std::collections::HashSet<(&PathBuf, &u64, &u64)> = entries
            .iter()
            .map(|e| (&e.path, &e.size, &e.modified_secs))
            .collect();
        cache.retain(|k, _| alive.contains(&(&k.0, &k.1, &k.2)));
    }
    (groups, stats, cache)
}

pub fn find_duplicates(entries: &[FileEntry]) -> Vec<DupeGroup> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let (groups, _, _) = find_duplicates_fast(
        entries,
        &DupeOptions::legacy(),
        None,
        &cancel,
        &|_, _| {},
    );
    groups
}

pub fn largest_files<'a>(index: &'a Index, n: usize) -> Vec<&'a FileEntry> {
    let mut v: Vec<&FileEntry> = index.entries.iter().collect();
    v.sort_by(|a, b| b.size.cmp(&a.size));
    v.truncate(n);
    v
}

pub fn top_level_sizes(index: &Index) -> Vec<(PathBuf, u64, usize)> {
    let mut map: HashMap<PathBuf, (u64, usize)> = HashMap::new();
    for e in &index.entries {
        let key = match e.path.strip_prefix(&index.root) {
            Ok(rel) => {
                let first = rel.components().next();
                match first {
                    Some(c) if rel.components().count() > 1 => index.root.join(c.as_os_str()),
                    _ => index.root.clone(),
                }
            }
            Err(_) => index.root.clone(),
        };
        let slot = map.entry(key).or_default();
        slot.0 += e.size;
        slot.1 += 1;
    }
    let mut v: Vec<(PathBuf, u64, usize)> =
        map.into_iter().map(|(k, (b, n))| (k, b, n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

pub fn fmt_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod zone_tests {
    use super::*;

    #[test]
    fn zone_classification() {
        assert_eq!(path_zone(Path::new(r"C:\Windows\System32\drivers\x.sys")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Windows\WinSxS\amd64\x.sys")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Program Files\AdGuard\a.wmv")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Program Files (x86)\App\a.dll")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Users\USERD030\.pinokio\vault\sha256\ab\blob")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\pinokio\bin\miniforge\pkgs\p\bash.exe")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Sandbox\USERD030\Nuke\drive\C\f.pak")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Users\USERD030\AppData\Local\NVIDIA\fingerprint.db")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Users\USERD030\.pionlogin\profiles\x\f.dat")), Zone::Protected);
        assert_eq!(path_zone(Path::new(r"C:\Users\USERD030\Documents\a.pdf")), Zone::User);
        assert_eq!(path_zone(Path::new(r"D:\Downloads\a.pdf")), Zone::User);
        assert!(group_all_user(&[
            PathBuf::from(r"C:\Users\u\Documents\a.pdf"),
            PathBuf::from(r"D:\backup\a.pdf"),
        ]));
        assert!(!group_all_user(&[
            PathBuf::from(r"C:\Users\u\Documents\a.pdf"),
            PathBuf::from(r"C:\Windows\a.pdf"),
        ]));

        assert!(!group_has_pinokio(&[
            PathBuf::from(r"C:\Users\u\Documents\a.pdf"),
            PathBuf::from(r"D:\backup\a.pdf"),
        ]));
        assert!(group_has_pinokio(&[
            PathBuf::from(r"C:\pinokio\api\Maestro.git\app\ckpts\minimax_h3\vae\minimax_h3_audio_vae_fp32.safetensors"),
            PathBuf::from(r"C:\pinokio\drive\drives\peers\d1787280396074\vae\minimax_h3_audio_vae_fp32.safetensors"),
        ]));
        assert_eq!(
            r"C:\pinokio\drive\drives\peers\d1787280396074\vae\minimax_h3_audio_vae_fp32.safetensors".len(),
            86
        );
        assert_eq!(
            r"C:\pinokio\api\Maestro.git\app\ckpts\minimax_h3\vae\minimax_h3_audio_vae_fp32.safetensors".len(),
            89
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tree() -> PathBuf {
        let base = std::env::temp_dir().join(format!("ffc-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/sub")).unwrap();
        fs::create_dir_all(base.join("empty1/nested")).unwrap();
        fs::write(base.join("a/hello.txt"), b"hello world").unwrap();
        fs::write(base.join("a/sub/hello-copy.txt"), b"hello world").unwrap();
        fs::write(base.join("a/other.txt"), b"something else entirely").unwrap();
        fs::write(base.join("big.bin"), vec![7u8; 5000]).unwrap();
        base
    }

    #[test]
    fn scan_search_dupe_disk() {
        let base = make_tree();
        let idx = scan(&base);
        assert_eq!(idx.entries.len(), 4);

        assert_eq!(search(&idx, "hello", 100).len(), 2);
        assert_eq!(search(&idx, "hello sub", 100).len(), 1);
        assert_eq!(search(&idx, "khong-co", 100).len(), 0);

        let dupes = find_duplicates(&idx.entries);
        assert_eq!(dupes.len(), 1);
        assert_eq!(dupes[0].paths.len(), 2);
        assert_eq!(dupes[0].wasted(), 11);

        assert!(idx.empty_dirs.iter().any(|d| d.ends_with("empty1")));

        let big = largest_files(&idx, 1);
        assert_eq!(big[0].name, "big.bin");
        let _ = fs::remove_dir_all(&base);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tier {

    Safe,

    Caution,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub title: String,
    pub tier: Tier,
    pub advice: String,

    pub deletable: bool,
    pub entries: Vec<FileEntry>,
}

impl Suggestion {
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

const DAY: u64 = 86_400;

fn older_than(e: &FileEntry, now: u64, days: u64) -> bool {
    e.modified_secs > 0 && now > e.modified_secs && now - e.modified_secs > days * DAY
}

pub fn analyze_cleanup(entries: &[FileEntry], now_secs: u64) -> Vec<Suggestion> {
    let mut temp_old: Vec<FileEntry> = Vec::new();
    let mut dumps: Vec<FileEntry> = Vec::new();
    let mut caches: Vec<FileEntry> = Vec::new();
    let mut logs_old: Vec<FileEntry> = Vec::new();
    let mut recycle: Vec<FileEntry> = Vec::new();
    let mut installers: Vec<FileEntry> = Vec::new();
    let mut big_old: Vec<FileEntry> = Vec::new();

    for e in entries {
        let p = e.path_lower.as_str();
        let n = e.name_lower.as_str();

        if p.contains("\\windows\\winsxs\\") || p.contains("\\windows\\installer\\") {
            continue;
        }

        if p.contains("$recycle.bin") {
            recycle.push(e.clone());
            continue;
        }
        if n.ends_with(".dmp") || n.ends_with(".mdmp") || p.contains("crashdumps") {
            dumps.push(e.clone());
            continue;
        }
        let in_cache_dir = p.contains("\\cache\\")
            || p.contains("\\gpucache\\")
            || p.contains("\\code cache\\")
            || p.contains("\\shadercache\\")
            || p.contains("\\dxcache\\")
            || p.contains("\\glcache\\")
            || p.contains("\\crashpad\\")
            || p.contains("\\thumbcachetodelete\\")
            || n.starts_with("thumbcache_")
            || n.starts_with("iconcache_");
        if in_cache_dir {
            caches.push(e.clone());
            continue;
        }
        let in_temp = p.contains("\\temp\\")
            || p.contains("\\tmp\\")
            || n.ends_with(".tmp")
            || n.ends_with(".temp");
        if in_temp && older_than(e, now_secs, 7) {
            temp_old.push(e.clone());
            continue;
        }
        if n.ends_with(".log") && older_than(e, now_secs, 30) {
            logs_old.push(e.clone());
            continue;
        }
        if p.contains("\\downloads\\")
            && (n.ends_with(".exe") || n.ends_with(".msi") || n.ends_with(".iso"))
            && e.size >= 50 * 1024 * 1024
            && older_than(e, now_secs, 90)
        {
            installers.push(e.clone());
            continue;
        }
        if e.size >= 500 * 1024 * 1024 && older_than(e, now_secs, 365) && p.contains("\\users\\") {
            big_old.push(e.clone());
            continue;
        }
    }

    let by_size_desc = |v: &mut Vec<FileEntry>| v.sort_by(|a, b| b.size.cmp(&a.size));
    by_size_desc(&mut temp_old);
    by_size_desc(&mut dumps);
    by_size_desc(&mut caches);
    by_size_desc(&mut logs_old);
    by_size_desc(&mut recycle);
    by_size_desc(&mut installers);
    by_size_desc(&mut big_old);
    big_old.truncate(50);

    let mut out: Vec<Suggestion> = Vec::new();
    let mut push = |title: &str, tier: Tier, advice: &str, deletable: bool, entries: Vec<FileEntry>| {
        if !entries.is_empty() {
            out.push(Suggestion {
                title: title.to_string(),
                tier,
                advice: advice.to_string(),
                deletable,
                entries,
            });
        }
    };
    push(
        "Temp files older than 7 days",
        Tier::Safe,
        "Temporary files programs create for short-term use. Only files untouched for over 7 days are listed (files in use are newer). If a needed file is deleted, the program recreates it. Windows servicing areas (WinSxS, Windows\\Installer) are fully excluded from this category.",
        true,
        temp_old,
    );
    push(
        "Crash dumps",
        Tier::Safe,
        "A .dmp file is created when a program crashes (e.g. Houdini_CrashDumps). It only helps when sent to the vendor for debugging — keeping it does not make the program run better.",
        true,
        dumps,
    );
    push(
        "Browser / app cache",
        Tier::Safe,
        "Chrome/Edge/app caches, GPU shader caches, thumbnail caches. Apps recreate them automatically; the first launch afterwards may be a bit slower.",
        true,
        caches,
    );
    push(
        "Log files older than 30 days",
        Tier::Safe,
        "Log files over 30 days old are almost never read again. Newer logs are kept.",
        true,
        logs_old,
    );
    push(
        "Recycle Bin",
        Tier::Safe,
        "These are items you deleted but Windows still keeps in $Recycle.Bin. To reclaim this space: right-click the Recycle Bin icon on the Desktop → Empty Recycle Bin (the app never empties it for you).",
        false,
        recycle,
    );
    push(
        "Old installers in Downloads (>90 days, >50 MB)",
        Tier::Caution,
        "Installer .exe/.msi/.iso files sitting in Downloads for a long time. Once the app is installed you usually no longer need its installer, but check each file yourself — the app never deletes this category for you.",
        false,
        installers,
    );
    push(
        "Very large files (>500 MB) untouched for over a year in user folders",
        Tier::Caution,
        "Candidates for moving to an external drive/archive HDD instead of the main SSD. This is personal data — the app only lists it, never deletes it.",
        false,
        big_old,
    );
    out
}

#[cfg(test)]
mod analyze_tests {
    use super::*;
    use std::path::PathBuf;

    fn fe(path_lower: &str, name_lower: &str, size: u64, modified_secs: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path_lower),
            name: name_lower.to_string(),
            name_lower: name_lower.to_string(),
            path_lower: path_lower.to_string(),
            size,
            modified_secs,
        }
    }

    #[test]
    fn cleanup_categories_classify_safely() {
        let now: u64 = 1_800_000_000;
        let old = now - 40 * DAY;
        let fresh = now - 1 * DAY;
        let entries = vec![

            fe("c:\\users\\x\\appdata\\local\\temp\\a.tmp", "a.tmp", 10, old),
            fe("c:\\users\\x\\appdata\\local\\temp\\b.tmp", "b.tmp", 10, fresh),

            fe("c:\\users\\x\\documents\\houdini_crashdumps\\h.dmp", "h.dmp", 99, fresh),

            fe("c:\\users\\x\\appdata\\local\\google\\chrome\\user data\\default\\cache\\c1", "c1", 5, fresh),

            fe("c:\\app\\new.log", "new.log", 5, fresh),
            fe("c:\\app\\old.log", "old.log", 5, old),

            fe("c:\\users\\x\\downloads\\setup.exe", "setup.exe", 60 * 1024 * 1024, now - 100 * DAY),

            fe("c:\\users\\x\\.pinokio\\vault\\sha256\\35\\35ab", "35ab", 15_000_000_000, fresh),

            fe("c:\\windows\\winsxs\\temp\\inflight\\x.tmp", "x.tmp", 10, old),
            fe("c:\\windows\\installer\\msi1234.tmp", "msi1234.tmp", 10, old),
        ];
        let s = analyze_cleanup(&entries, now);
        let by_title = |t: &str| s.iter().find(|x| x.title.starts_with(t));
        assert_eq!(by_title("Temp files").map(|x| x.entries.len()), Some(1));
        assert_eq!(by_title("Crash dump").map(|x| x.entries.len()), Some(1));
        assert_eq!(by_title("Browser / app cache").map(|x| x.entries.len()), Some(1));
        assert_eq!(by_title("Log files").map(|x| x.entries.len()), Some(1));
        let inst = by_title("Old installers").expect("must have an installers category");
        assert_eq!(inst.tier, Tier::Caution);
        assert!(!inst.deletable);

        assert!(s.iter().all(|x| x.entries.iter().all(|e| !e.path_lower.contains("vault"))));

        assert!(s
            .iter()
            .all(|x| x.entries.iter().all(|e| !e.path_lower.contains("winsxs") && !e.path_lower.contains("installer"))));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {

    User,

    Protected,
}

pub fn path_zone(path: &Path) -> Zone {
    let s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    const MARKERS: &[&str] = &[
        "\\windows\\",
        "\\program files",
        "\\programdata\\",
        "\\$recycle.bin\\",
        "\\.pinokio\\vault\\",
        "\\appdata\\",
        "\\.pionlogin\\",
        "\\.pinokio\\",
        "\\miniforge\\",
        "\\miniconda",
        "\\anaconda",
        "\\sandbox\\",
    ];
    if MARKERS.iter().any(|m| s.contains(m)) {
        Zone::Protected
    } else {
        Zone::User
    }
}

pub fn group_all_user(paths: &[PathBuf]) -> bool {
    paths.iter().all(|p| path_zone(p) == Zone::User)
}

pub fn group_has_pinokio(paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| {
        p.to_string_lossy()
            .replace('/', "\\")
            .to_lowercase()
            .contains("pinokio")
    })
}

pub fn refresh_index(index: &mut Index, removed: &std::collections::HashSet<PathBuf>) {
    if removed.is_empty() {
        return;
    }
    index.entries.retain(|e| !removed.contains(&e.path));
    index.total_bytes = index.entries.iter().map(|e| e.size).sum();

    let mut has: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for e in &index.entries {
        if let Some(p) = e.path.parent() {
            has.insert(p.to_path_buf());
        }
    }
    let mut frontier: Vec<PathBuf> = has.iter().cloned().collect();
    while let Some(d) = frontier.pop() {
        if let Some(p) = d.parent() {
            if has.insert(p.to_path_buf()) {
                frontier.push(p.to_path_buf());
            }
        }
    }

    let mut empty_set: std::collections::HashSet<PathBuf> = index
        .empty_dirs
        .iter()
        .filter(|d| !has.contains(*d))
        .cloned()
        .collect();

    for f in removed {
        let mut cur = f.parent().map(|p| p.to_path_buf());
        while let Some(d) = cur {
            if d == index.root || !d.starts_with(&index.root) {
                break;
            }
            if !has.contains(&d) {
                empty_set.insert(d.clone());
            }
            cur = d.parent().map(|p| p.to_path_buf());
        }
    }
    let mut empties: Vec<PathBuf> = empty_set.into_iter().collect();
    empties.sort();
    index.empty_dirs = empties;
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn refresh_removes_entries_and_finds_new_empty_dirs() {
        let base = std::env::temp_dir().join(format!("ffc-refresh-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/b/f1.txt"), b"x").unwrap();
        std::fs::write(base.join("a/f2.txt"), b"yy").unwrap();
        let mut idx = scan(&base);
        assert_eq!(idx.entries.len(), 2);
        assert!(idx.empty_dirs.is_empty());

        let mut removed = HashSet::new();
        removed.insert(base.join("a/b/f1.txt"));
        removed.insert(base.join("a/f2.txt"));

        refresh_index(&mut idx, &removed);
        assert_eq!(idx.entries.len(), 0);
        assert_eq!(idx.total_bytes, 0);
        assert!(idx.empty_dirs.contains(&base.join("a/b")));
        assert!(idx.empty_dirs.contains(&base.join("a")));
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod fast_engine_tests {
    use super::*;

    #[test]
    fn fast_engine_and_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ffc-fast-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let content_a = vec![7u8; 5000];
        let mut content_b = vec![7u8; 5000];
        content_b[2500] = 9;
        std::fs::write(dir.join("a1.bin"), &content_a).unwrap();
        std::fs::write(dir.join("a2.bin"), &content_a).unwrap();
        std::fs::write(dir.join("b.bin"), &content_b).unwrap();
        let idx = scan(&dir);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let opts = DupeOptions::default();
        let (groups, st, cache) =
            find_duplicates_fast(&idx.entries, &opts, None, &cancel, &|_, _| {});
        assert_eq!(groups.len(), 1, "only the a1/a2 pair is a true duplicate");
        assert_eq!(groups[0].paths.len(), 2);
        assert!(st.hashed >= 3, "first run must really hash: {:?}", st);
        assert_eq!(st.cache_hits, 0);

        let cf = dir.join("cache.bin");
        save_hash_cache(&cf, &cache).unwrap();
        let cache2 = load_hash_cache(&cf);
        assert_eq!(cache2.len(), cache.len());

        let (groups2, st2, _) =
            find_duplicates_fast(&idx.entries, &opts, Some(cache2), &cancel, &|_, _| {});
        assert_eq!(groups2.len(), 1);
        assert_eq!(st2.hashed, 0, "second run must be all cache hits: {:?}", st2);
        assert!(st2.cache_hits >= 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardlink_groups_detected_and_excluded_from_wasted() {
        let dir = std::env::temp_dir().join(format!("ffc-hardlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let content = vec![3u8; 5000];
        std::fs::write(dir.join("orig.bin"), &content).unwrap();

        std::fs::hard_link(dir.join("orig.bin"), dir.join("link.bin")).unwrap();

        std::fs::write(dir.join("copy.bin"), &content).unwrap();
        let idx = scan(&dir);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let opts = DupeOptions::default();
        let (groups, st, _) =
            find_duplicates_fast(&idx.entries, &opts, None, &cancel, &|_, _| {});
        assert_eq!(groups.len(), 1, "all 3 paths share content -> 1 group");
        let g = &groups[0];
        assert_eq!(g.paths.len(), 3);

        assert!(!g.hardlinked);
        assert_eq!(g.wasted(), 5000);

        let dir2 = std::env::temp_dir().join(format!("ffc-hardlink2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir2);
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("a.bin"), &content).unwrap();
        std::fs::hard_link(dir2.join("a.bin"), dir2.join("b.bin")).unwrap();
        let idx2 = scan(&dir2);
        let (groups2, st2, _) =
            find_duplicates_fast(&idx2.entries, &opts, None, &cancel, &|_, _| {});
        assert_eq!(groups2.len(), 1);
        assert!(groups2[0].hardlinked, "two names for one file must be labeled hardlink");
        assert_eq!(groups2[0].wasted(), 0, "hardlink uses no extra space");
        assert_eq!(st2.hardlink_groups, 1);
        assert_eq!(st2.hardlink_bytes, 5000);
        let _ = st.hardlink_groups;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}

const DUPES_MAGIC: &[u8; 5] = b"TFDP1";

pub fn save_dupes(path: &Path, groups: &[DupeGroup]) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(groups.len() * 200);
    buf.extend_from_slice(DUPES_MAGIC);
    put_u32(&mut buf, groups.len() as u32);
    for g in groups {
        put_u64(&mut buf, g.size);
        put_str(&mut buf, &g.hash);
        buf.push(if g.hardlinked { 1 } else { 0 });
        put_u64(&mut buf, g.distinct_files as u64);
        put_u32(&mut buf, g.paths.len() as u32);
        for p in &g.paths {
            put_str(&mut buf, &p.to_string_lossy());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, path)
}

pub fn load_dupes(path: &Path) -> Vec<DupeGroup> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if data.len() < 9 || &data[..5] != DUPES_MAGIC {
        return Vec::new();
    }
    let mut cur = 5usize;
    let rd_u32 = |d: &[u8], c: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(d.get(*c..*c + 4)?.try_into().ok()?);
        *c += 4;
        Some(v)
    };
    let rd_u64 = |d: &[u8], c: &mut usize| -> Option<u64> {
        let v = u64::from_le_bytes(d.get(*c..*c + 8)?.try_into().ok()?);
        *c += 8;
        Some(v)
    };
    let rd_str = |d: &[u8], c: &mut usize| -> Option<String> {
        let n = rd_u32(d, c)? as usize;
        let b = d.get(*c..*c + n)?;
        *c += n;
        Some(String::from_utf8_lossy(b).into_owned())
    };
    let parse = |d: &[u8], c: &mut usize| -> Option<Vec<DupeGroup>> {
        let n = rd_u32(d, c)? as usize;
        let mut out: Vec<DupeGroup> = Vec::with_capacity(n.min(1_000_000));
        for _ in 0..n {
            let size = rd_u64(d, c)?;
            let hash = rd_str(d, c)?;
            let hardlinked = *d.get(*c)? == 1;
            *c += 1;
            let distinct_files = rd_u64(d, c)? as usize;
            let np = rd_u32(d, c)? as usize;
            let mut paths: Vec<PathBuf> = Vec::with_capacity(np.min(100_000));
            for _ in 0..np {
                paths.push(PathBuf::from(rd_str(d, c)?));
            }
            out.push(DupeGroup {
                size,
                hash,
                paths,
                hardlinked,
                distinct_files,
            });
        }
        Some(out)
    };
    parse(&data, &mut cur).unwrap_or_default()
}

#[cfg(test)]
mod index_store_tests {
    use super::*;

    #[test]
    fn index_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ffc-idx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("empty1")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("sub").join("b.bin"), vec![0u8; 1234]).unwrap();
        let idx = scan(&dir);
        let f = dir.join("idx.bin");
        save_index(&f, &idx, 1_700_000_000).unwrap();
        let (back, at) = load_index(&f).expect("saved index loads");
        assert_eq!(at, 1_700_000_000);
        assert_eq!(back.root, idx.root);
        assert_eq!(back.entries.len(), idx.entries.len());
        assert_eq!(back.total_bytes, idx.total_bytes);
        assert!(back.entries.iter().any(|e| e.name == "b.bin" && e.size == 1234));
        assert!(back
            .empty_dirs
            .iter()
            .any(|d| d.ends_with("empty1")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod dupes_store_tests {
    use super::*;

    #[test]
    fn dupes_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ffc-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let groups = vec![
            DupeGroup {
                size: 100,
                hash: "abc".to_string(),
                paths: vec![dir.join("a.bin"), dir.join("b.bin")],
                hardlinked: false,
                distinct_files: 2,
            },
            DupeGroup {
                size: 50,
                hash: "def".to_string(),
                paths: vec![dir.join("x.bin"), dir.join("y.bin")],
                hardlinked: true,
                distinct_files: 1,
            },
        ];
        let f = dir.join("dup.bin");
        save_dupes(&f, &groups).unwrap();
        let back = load_dupes(&f);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].paths.len(), 2);
        assert_eq!(back[0].wasted(), 100);
        assert!(back[1].hardlinked);
        assert_eq!(back[1].wasted(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
