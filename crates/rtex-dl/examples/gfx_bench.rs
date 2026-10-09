//! `cargo run --release -p rtex-dl --example gfx_bench -- PAGE.json...`: per page, the time to
//! decode its binary display list and interpret it for native drawing (median of 21 runs).
use rtex_dl::DisplayList;
use std::time::Instant;

fn main() {
    let mut rows = Vec::new();
    for path in std::env::args().skip(1) {
        let dl: DisplayList = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        if !rtex_dl::gfx::only_literals(&dl) {
            continue;
        }
        let bin = dl.to_binary();
        if let (Ok(_), Err(e)) = (rtex_dl::gfx::native_graphics(&dl), rtex_dl::gfx::native_graphics(&DisplayList::from_binary(&bin).unwrap())) {
            println!("BINARY ROUND TRIP LOSES NATIVE DRAWING: {path}: {e}");
        }
        let mut times = Vec::new();
        let mut ops = 0;
        for _ in 0..21 {
            let t = Instant::now();
            let d = DisplayList::from_binary(&bin).unwrap();
            let n = rtex_dl::gfx::native_graphics(&d);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            ops = n.map(|n| n.ops.len()).unwrap_or(0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        rows.push((times[10], ops, bin.len(), path));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    for (ms, ops, bytes, path) in rows.iter().take(5) {
        println!("{ms:.3} ms  ops {ops}  binary {bytes} B  {path}");
    }
}
