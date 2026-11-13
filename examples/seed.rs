use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.to_lowercase().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let root = PathBuf::from("/tmp/fixture");
    let out = PathBuf::from("/tmp/fakedata/FileSweep");
    let idx = filesweep::scan(&root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let h = fnv(&root.to_string_lossy());
    filesweep::save_index(&out.join(format!("index-{h:016x}.bin")), &idx, now).unwrap();
    let opts = filesweep::DupeOptions { min_size: 0, skip_protected: false, threads: 2 };
    let cancel = AtomicBool::new(false);
    let (groups, _st, _c) = filesweep::find_duplicates_fast(&idx.entries, &opts, None, &cancel, &|_, _| {});
    filesweep::save_dupes(&out.join(format!("dupes-{h:016x}.bin")), &groups).unwrap();
    println!("seeded {} entries, {} groups", idx.entries.len(), groups.len());
}
