//! Micro-benchmark for the hot paths. Run: `cargo run --release --example bench`.
//!
//! Not a criterion suite (kept dependency-free); prints wall times so a perf
//! regression in claim/complete/open_sweep is visible at a glance.
use std::time::Instant;
use trail_core::{Config, FolderStat, NextResult, Store, WorkStatus};

fn folders(n: usize) -> Vec<FolderStat> {
    (0..n)
        .map(|i| FolderStat {
            path: format!("area{:03}/mod{:05}", i / 100, i),
            file_count: (i % 7 + 1) as i64,
            size_bytes: 100,
            churn: 0,
        })
        .collect()
}

fn drain(s: &mut Store, cfg: &Config, now: i64, found: Option<i64>) -> u64 {
    let mut n = 0;
    while let NextResult::Ok { path, .. } = s.next("t", cfg, Some("a"), None, false, now).unwrap() {
        s.complete("t", &path, Some("a"), WorkStatus::Done, None, found, now)
            .unwrap();
        n += 1;
    }
    n
}

fn main() {
    let now = 1_000_000i64;
    let cfg = Config::default();

    // Drain throughput vs folder count (should be ~flat us/cycle => O(N) total).
    for &n in &[1_000usize, 10_000, 50_000] {
        let mut s = Store::open_in_memory().unwrap();
        let fs = folders(n);

        let t = Instant::now();
        s.replace_folders(&fs, now).unwrap();
        let replace = t.elapsed();

        let t = Instant::now();
        s.next("t", &cfg, Some("a"), None, false, now).unwrap(); // bootstrap + open_sweep
        let open = t.elapsed();

        let t = Instant::now();
        let cnt = drain(&mut s, &cfg, now, None) + 1;
        let drain_dt = t.elapsed();
        println!(
            "n={n:>6}: replace={replace:>9.2?} open_sweep={open:>9.2?} drain {cnt} in {drain_dt:>9.2?} ({:.2} us/cycle)",
            drain_dt.as_micros() as f64 / cnt as f64
        );
    }

    // complete() latency as accumulated sweeps grow (exercises idx_work_path).
    let mut s = Store::open_in_memory().unwrap();
    s.replace_folders(&folders(1), now).unwrap();
    let path = "area000/mod00000".to_string();
    for k in 0..200i64 {
        s.next("t", &cfg, Some("a"), None, true, now + k).unwrap();
        s.complete("t", &path, Some("a"), WorkStatus::Done, None, None, now + k)
            .unwrap();
    }
    s.open_new_sweep("t", &cfg, now + 1000).unwrap();
    s.next("t", &cfg, Some("a"), None, false, now + 1000)
        .unwrap();
    let t = Instant::now();
    s.complete(
        "t",
        &path,
        Some("a"),
        WorkStatus::Done,
        None,
        None,
        now + 2000,
    )
    .unwrap();
    println!("complete() after 200 accumulated sweeps: {:?}", t.elapsed());

    // open_sweep cost over 2000 folders as visit history accumulates.
    let mut s = Store::open_in_memory().unwrap();
    s.replace_folders(&folders(2_000), now).unwrap();
    s.open_new_sweep("t", &cfg, now).unwrap();
    for sweep in 1..=16i64 {
        let n = drain(&mut s, &cfg, now + sweep, Some(1));
        let t = Instant::now();
        let info = s.open_new_sweep("t", &cfg, now + sweep).unwrap();
        if sweep % 5 == 1 {
            println!(
                "open_sweep #{} over {n} folders, {sweep} sweeps of history: {:?}",
                info.sweep,
                t.elapsed()
            );
        }
    }
}
