//! zap CLI：文件级压缩/解压/自检。核心逻辑在 lib.rs（同一套代码也给手机 App 用）。

use std::fs;
use std::time::Instant;
use zap::*;

/// 三档预设：(名字, 阶数, 每线程内存 MB, 块大小 MB)
const LEVELS: [(&str, u32, u32, usize); 3] =
    [("fast", 5, 24, 8), ("std", 6, 96, 32), ("max", 8, 512, 128)];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    let level = std::env::var("ZAP_LEVEL").unwrap_or_else(|_| "std".to_string());
    let lv = LEVELS
        .iter()
        .find(|l| l.0 == level.as_str())
        .copied()
        .unwrap_or(LEVELS[1]);
    let order: u32 = envu("ZAP_ORDER", lv.1);
    let mem_mb: u32 = envu("ZAP_MEM", lv.2);
    let bsz_mb: usize = envu("ZAP_BSZ", lv.3);

    let par_idx = if cmd == "t" { 3 } else { 4 };
    let par: usize = args
        .get(par_idx)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
    match cmd {
        "c" => {
            let len = fs::metadata(&args[2]).expect("stat").len() as usize;
            let bsz = if bsz_mb == 0 {
                auto_bsz(len, par)
            } else {
                bsz_mb << 20
            };
            let t0 = Instant::now();
            let (n, out_len) = compress_file(&args[2], &args[3], par, order, mem_mb, bsz);
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "zap c {} -> {} in={} out={} bpb={:.4} level={} order={} mem={}MB threads={} block={}MB speed={:.2}MB/s {:.1}s",
                args[2], args[3], n, out_len,
                out_len as f64 * 8.0 / n.max(1) as f64,
                lv.0, order, mem_mb, par, bsz >> 20, n as f64 / 1e6 / secs, secs
            );
        }
        "d" => {
            let t0 = Instant::now();
            let n = decompress_file(&args[2], &args[3], par);
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "zap d -> {} bytes threads={} speed={:.2}MB/s {:.1}s",
                n,
                par,
                n as f64 / 1e6 / secs,
                secs
            );
        }
        "t" => {
            let len = fs::metadata(&args[2]).expect("stat").len() as usize;
            let bsz = if bsz_mb == 0 {
                auto_bsz(len, par)
            } else {
                bsz_mb << 20
            };
            let t1 = std::env::temp_dir().join("zap_a.bin");
            let t2 = std::env::temp_dir().join("zap_b.bin");
            let t0 = Instant::now();
            let (n, out_len) =
                compress_file(&args[2], t1.to_str().unwrap(), par, order, mem_mb, bsz);
            let enc = t0.elapsed().as_secs_f64();
            let t1b = Instant::now();
            decompress_file(t1.to_str().unwrap(), t2.to_str().unwrap(), par);
            let dec = t1b.elapsed().as_secs_f64();
            let ok = files_equal(&args[2], t2.to_str().unwrap());
            let _ = fs::remove_file(&t1);
            let _ = fs::remove_file(&t2);
            println!(
                "zap t {} in={} out={} bpb={:.4} lossless={} level={} threads={} block={}MB enc={:.1}s({:.2}MB/s) dec={:.1}s({:.2}MB/s)",
                args[2], n, out_len, out_len as f64 * 8.0 / n.max(1) as f64, ok,
                lv.0, par, bsz >> 20,
                enc, n as f64 / 1e6 / enc, dec, n as f64 / 1e6 / dec
            );
        }
        _ => {
            println!("zap: c <in> <out> [threads] | d <in> <out> [threads] | t <in> [threads]");
            println!(
                "env: ZAP_LEVEL(fast|std|max) ZAP_ORDER ZAP_MEM(每线程 MB) ZAP_BSZ(MB, 0=自动)"
            );
            for (name, o, m, b) in LEVELS {
                println!("  ZAP_LEVEL={name}: order={o} mem={m}MB block={b}MB");
            }
        }
    }
}
