//! Measurement harness: keystroke -> echoed cell in the grid.
//!
//! Measures the part of the input pipeline that does not involve the GUI:
//! `Session::write` -> ConPTY -> child echo -> alacritty parse -> `snapshot`.
//! Run idle and under CPU load to compare.
//!
//! `cargo run -p illness-term --example latency_probe --release -- <label> <samples> [pidfile]`

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use illness_core::CursorKind;
use illness_term::{GridSize, Session, SpawnSpec, snapshot::Snapshot};

fn row_text(snapshot: &Snapshot, row: usize) -> String {
    let mut out = String::new();
    for col in 0..snapshot.cols {
        let cell = snapshot.cells[row * snapshot.cols + col];
        out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
    }
    out
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn main() {
    let label = std::env::args().nth(1).unwrap_or_else(|| "unloaded".into());
    let samples: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let pidfile = std::env::args().nth(3);

    let spec = SpawnSpec {
        program: "cmd.exe".into(),
        args: vec!["/Q".into()],
        cwd: None,
        env: Vec::new(),
        scrollback: 10_000,
        cursor: CursorKind::Block,
    };
    let session = Session::spawn(spec, GridSize::new(120, 40), (8, 16), Arc::new(|| {}))
        .expect("spawn conpty session");
    println!("child pid {:?}", session.child_pid());

    if let Some(path) = &pidfile {
        let _ = std::fs::write(path, format!("{}", session.child_pid().unwrap_or(0)));
        // Give the orchestrator time to apply external settings.
        std::thread::sleep(Duration::from_millis(1500));
    }

    // Consume the banner until the prompt has settled.
    std::thread::sleep(Duration::from_millis(1200));
    let _ = session.snapshot();

    let mut timings = Vec::with_capacity(samples);
    let mut misses = 0;
    for i in 0..samples {
        // Fresh prompt, then one burst so the measured latency is the echo of
        // the last byte in the burst.
        session.write(b"\r");
        std::thread::sleep(Duration::from_millis(120));
        let token = format!("z{i:03}z");
        let start = Instant::now();
        session.write(token.as_bytes());
        let mut seen = false;
        while start.elapsed() < Duration::from_secs(3) {
            let _ = session.events().recv_timeout(Duration::from_micros(500));
            let snapshot = session.snapshot();
            if (0..snapshot.rows).any(|row| row_text(&snapshot, row).contains(&token)) {
                seen = true;
                break;
            }
        }
        if seen {
            timings.push(start.elapsed());
        } else {
            misses += 1;
        }
    }

    let mut sorted = timings.clone();
    sorted.sort();
    let total: Duration = timings.iter().sum();
    let mean = if timings.is_empty() {
        Duration::ZERO
    } else {
        total / timings.len() as u32
    };
    let mut buckets: BTreeMap<u64, usize> = BTreeMap::new();
    for t in &timings {
        *buckets.entry(t.as_millis() as u64 / 10 * 10).or_insert(0) += 1;
    }

    println!("== {label} ==");
    println!("samples: {}/{} (misses {misses})", timings.len(), samples);
    println!("mean:    {:.2} ms", mean.as_secs_f64() * 1e3);
    println!(
        "p50:     {:.2} ms",
        percentile(&sorted, 0.50).as_secs_f64() * 1e3
    );
    println!(
        "p95:     {:.2} ms",
        percentile(&sorted, 0.95).as_secs_f64() * 1e3
    );
    println!(
        "max:     {:.2} ms",
        percentile(&sorted, 1.0).as_secs_f64() * 1e3
    );
    println!("histogram (10ms buckets): {buckets:?}");
}
