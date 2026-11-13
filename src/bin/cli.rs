use std::path::PathBuf;

use filesweep::{find_duplicates, fmt_size, largest_files, scan, search, top_level_sizes};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("Usage: filesweep-cli <folder> [--search <keyword>] [--dupes] [--largest <n>]");
        return;
    }
    let root = PathBuf::from(&args[0]);
    let idx = scan(&root);
    println!(
        "Scanned {}: {} files, {}, in {} ms",
        root.display(),
        idx.entries.len(),
        fmt_size(idx.total_bytes),
        idx.scan_ms
    );

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--search" => {
                let q = args.get(i + 1).cloned().unwrap_or_default();
                let hits = search(&idx, &q, 50);
                println!("\n== Search \"{}\" : {} results (showing at most 50) ==", q, hits.len());
                for e in hits {
                    println!("{:>12}  {}", fmt_size(e.size), e.path.display());
                }
                i += 2;
            }
            "--dupes" => {
                let dupes = find_duplicates(&idx.entries);
                let wasted: u64 = dupes.iter().map(|g| g.wasted()).sum();
                println!(
                    "\n== Duplicates: {} groups, {} wasted ==",
                    dupes.len(),
                    fmt_size(wasted)
                );
                for g in dupes.iter().take(30) {
                    println!("-- {} x{} (wasted {}){}", fmt_size(g.size), g.paths.len(), fmt_size(g.wasted()), if g.hardlinked { " — hardlink, same physical data so it uses no extra space" } else { "" });
                    for p in &g.paths {
                        println!("   {}", p.display());
                    }
                }
                i += 1;
            }
            "--largest" => {
                let n: usize = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(20);
                println!("\n== {} largest files ==", n);
                for e in largest_files(&idx, n) {
                    println!("{:>12}  {}", fmt_size(e.size), e.path.display());
                }
                println!("\n== Size by top-level subfolder ==");
                for (dir, bytes, count) in top_level_sizes(&idx) {
                    println!("{:>12}  ({} file)  {}", fmt_size(bytes), count, dir.display());
                }
                if !idx.empty_dirs.is_empty() {
                    println!("\n== Empty folders: {} ==", idx.empty_dirs.len());
                    for d in idx.empty_dirs.iter().take(30) {
                        println!("   {}", d.display());
                    }
                }
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
}
