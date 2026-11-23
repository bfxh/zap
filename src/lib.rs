//! zap —— 可移植、多线程、分块并行的 PPMd 压缩器
//!
//! 单线程 PPMd 已经在 9 MB/s 量级；把输入切成块、每块独立建模型、多核并行，
//! 速度随核数线性上升，代价是每块重新学习（块越大，损失越小）。
//!
//! 同一套代码编译成 CLI（`zap`）、静态/动态库（手机 App 通过 FFI 调用）。
//!
//! 设计要点（v0.2）：
//! - 每个线程只建一次模型存储，块与块之间 `reset` 复用，不再逐块 malloc/free；
//! - 压缩结果按偏移直接写进输出文件，不再把整包压缩数据攒在内存里；
//! - 解码认 ZAP2（索引带偏移）与旧版 ZAP1（流按块序隐式排列）。
//!
//! 用法：
//!   zap c <in> <out> [threads]
//!   zap d <in> <out> [threads]
//!   zap t <in> [threads]
//!
//! 环境变量：
//!   ZAP_ORDER  模型阶数（默认 6）
//!   ZAP_MEM    每线程模型内存 MB（默认 64；手机建议 32-64）
//!   ZAP_BSZ    块大小 MB（默认自动：总长/(线程×2)，夹在 4..32MB）

use ppmd_turbo::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// 流头长度：magic(4) + nblocks(4) + total(8) + order(1) + mem_mb(2)
pub const HEADER_LEN: usize = 19;
/// ZAP2 每块目录项：blen u32 + clen u32 + off u64
pub const INDEX_ENTRY: usize = 16;
/// ZAP1 每块目录项（旧格式，流按块序隐式拼接）
pub const INDEX_ENTRY_V1: usize = 8;
pub const MAGIC: &[u8; 4] = b"ZAP2";
pub const MAGIC_V1: &[u8; 4] = b"ZAP1";

pub fn envu<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn block_ranges(len: usize, bsz: usize) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut o = 0usize;
    while o < len {
        let e = (o + bsz).min(len);
        v.push((o, e));
        o = e;
    }
    if v.is_empty() {
        v.push((0, 0));
    }
    v
}

fn params_for(order: u32, mem_mb: u32, len: usize) -> Params {
    Params::new(order, mem_mb << 20)
        .expect("params")
        .reduced_for(len as u64)
}

/// 把 `enc` 里当前这个流跑完，返回压缩字节。
/// 调用方负责在这之前 `new` / `reset`（模型存储由 encoder 自己复用）。
fn drain_encoder(enc: &mut SevenZEncoder, data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; data.len() + data.len() / 4 + 65536];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut stalls = 0u32;
    while in_pos < data.len() {
        let p = enc
            .encode(&data[in_pos..], &mut out[out_pos..])
            .map_err(|e| format!("encode: {e:?}"))?;
        in_pos += p.consumed;
        out_pos += p.produced;
        if p.consumed == 0 && p.produced == 0 {
            stalls += 1;
            if stalls > 8 {
                return Err(format!("编码无进度（{} 字节输入）", data.len()));
            }
            out.resize(out.len().saturating_mul(2), 0);
        }
    }
    loop {
        let fin = enc
            .finish(&mut out[out_pos..], false)
            .map_err(|e| format!("finish: {e:?}"))?;
        out_pos += fin.produced;
        if fin.done {
            break;
        }
        if fin.produced == 0 {
            out.resize(out.len().saturating_mul(2), 0);
        }
    }
    out.truncate(out_pos);
    Ok(out)
}

/// 单块压缩（panic 版，一次性调用用）。
pub fn encode_block(data: &[u8], order: u32, mem_mb: u32) -> Vec<u8> {
    encode_block_try(data, order, mem_mb).expect("encode")
}

/// 单块压缩，返回错误而不是 panic。
///
/// 输出缓冲区按需翻倍增长：PPMd 对不可压缩的数据会略微膨胀，而编码器在输出
/// 缓冲填满时会返回 0 进度。老版本遇到这种情况直接 break，等于静默把尾巴截断
/// —— 那是一份悄悄损坏的归档，所以这里改成"没进度就扩容，翻倍 8 次还不行才报错"。
pub fn encode_block_try(data: &[u8], order: u32, mem_mb: u32) -> Result<Vec<u8>, String> {
    let params = params_for(order, mem_mb, data.len());
    let mut enc = SevenZEncoder::new(params).map_err(|e| format!("encoder: {e:?}"))?;
    drain_encoder(&mut enc, data)
}

/// 单块解压（panic 版，一次性调用用）。
pub fn decode_block(packed: &[u8], blen: usize, order: u32, mem_mb: u32) -> Vec<u8> {
    decode_block_try(packed, blen, order, mem_mb).expect("decode")
}

/// 单块解压，返回错误而不是 panic。
pub fn decode_block_try(
    packed: &[u8],
    blen: usize,
    order: u32,
    mem_mb: u32,
) -> Result<Vec<u8>, String> {
    let params = params_for(order, mem_mb, blen);
    let mut dec =
        SevenZDecoder::new(params, Some(blen as u64)).map_err(|e| format!("decoder: {e:?}"))?;
    drain_decoder(&mut dec, packed, blen)
}

fn drain_decoder(dec: &mut SevenZDecoder, packed: &[u8], blen: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; blen];
    let step = dec
        .decode(packed, true, &mut out)
        .map_err(|e| format!("decode: {e:?}"))?;
    if step.status != SevenZStatus::ReachedSize {
        return Err(format!(
            "块未按长度结束：期望 {blen} 字节，状态 {:?}",
            step.status
        ));
    }
    Ok(out)
}

/// 读一块（按偏移），避免把整块文件读进内存后再切片。
fn read_range(f: &mut fs::File, off: usize, n: usize) -> std::io::Result<Vec<u8>> {
    let mut data = vec![0u8; n];
    if n > 0 {
        f.seek(SeekFrom::Start(off as u64))?;
        f.read_exact(&mut data)?;
    }
    Ok(data)
}

/// 压缩文件。返回 (原始字节数, 输出字节数)。
///
/// 线程里只建一次 encoder（模型存储跟着复用），块与块之间 `reset`；
/// 压缩结果按"谁先完成谁先占位"的偏移直接写进输出文件，内存里只留索引。
pub fn compress_file(
    inp: &str,
    outp: &str,
    threads: usize,
    order: u32,
    mem_mb: u32,
    bsz: usize,
) -> (usize, usize) {
    let len = fs::metadata(inp).expect("stat").len() as usize;
    let ranges = block_ranges(len, bsz);
    let nb = ranges.len();
    let data_start = HEADER_LEN + nb * INDEX_ENTRY;

    let out_file = fs::File::create(outp).expect("create");
    out_file.set_len(data_start as u64).expect("set_len");
    let sink = Mutex::new(out_file);
    let cursor = AtomicUsize::new(data_start);
    let index: Mutex<Vec<(usize, u32, u32, u64)>> = Mutex::new(Vec::with_capacity(nb));
    let next = AtomicUsize::new(0);

    std::thread::scope(|sc| {
        for _ in 0..threads.max(1).min(nb) {
            sc.spawn(|| {
                let mut enc: Option<SevenZEncoder> = None;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= nb {
                        break;
                    }
                    let (a, b) = ranges[i];
                    let mut f = fs::File::open(inp).expect("open");
                    let data = read_range(&mut f, a, b - a).expect("read");
                    drop(f);

                    let params = params_for(order, mem_mb, data.len());
                    let packed = match enc.as_mut() {
                        Some(e) => {
                            e.reset(params).expect("reset");
                            drain_encoder(e, &data).expect("encode")
                        }
                        None => {
                            let mut e = SevenZEncoder::new(params).expect("encoder");
                            let packed = drain_encoder(&mut e, &data).expect("encode");
                            enc = Some(e);
                            packed
                        }
                    };

                    let off = cursor.fetch_add(packed.len(), Ordering::Relaxed);
                    {
                        let mut w = sink.lock().unwrap();
                        w.seek(SeekFrom::Start(off as u64)).expect("seek out");
                        w.write_all(&packed).expect("write block");
                    }
                    index.lock().unwrap().push((
                        i,
                        (b - a) as u32,
                        packed.len() as u32,
                        off as u64,
                    ));
                }
            });
        }
    });

    let mut idx = index.into_inner().unwrap();
    idx.sort_by_key(|x| x.0);

    let mut hdr = Vec::with_capacity(data_start);
    hdr.extend_from_slice(MAGIC);
    hdr.extend_from_slice(&(nb as u32).to_le_bytes());
    hdr.extend_from_slice(&(len as u64).to_le_bytes());
    hdr.push(order as u8);
    hdr.extend_from_slice(&(mem_mb as u16).to_le_bytes());
    let mut total = data_start;
    for (_, blen, clen, off) in &idx {
        hdr.extend_from_slice(&blen.to_le_bytes());
        hdr.extend_from_slice(&clen.to_le_bytes());
        hdr.extend_from_slice(&off.to_le_bytes());
        total += *clen as usize;
    }
    {
        let mut w = sink.into_inner().unwrap();
        w.seek(SeekFrom::Start(0)).expect("seek head");
        w.write_all(&hdr).expect("write head");
        w.flush().expect("flush");
    }
    (len, total)
}

/// 解压文件。认 ZAP2（索引带偏移）与旧版 ZAP1（流按块序隐式排列）。
pub fn decompress_file(inp: &str, outp: &str, threads: usize) -> usize {
    let mut f = fs::File::open(inp).expect("open");
    let mut hdr = [0u8; HEADER_LEN];
    f.read_exact(&mut hdr).expect("read header");
    let magic = &hdr[0..4];
    let v1 = magic == MAGIC_V1;
    if !v1 && magic != MAGIC {
        panic!("不是 ZAP1 / ZAP2 格式：{:?}", magic);
    }
    let nb = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(hdr[8..16].try_into().unwrap()) as usize;
    let order = hdr[16] as u32;
    let mem_mb = u16::from_le_bytes(hdr[17..19].try_into().unwrap()) as u32;

    let entry = if v1 { INDEX_ENTRY_V1 } else { INDEX_ENTRY };
    let mut blocks = Vec::with_capacity(nb);
    let mut off = HEADER_LEN + nb * entry;
    let mut out_off = 0usize;
    for _ in 0..nb {
        let mut e = [0u8; 16];
        f.read_exact(&mut e[..entry]).expect("read index");
        let blen = u32::from_le_bytes(e[0..4].try_into().unwrap()) as usize;
        let clen = u32::from_le_bytes(e[4..8].try_into().unwrap()) as usize;
        let at = if v1 {
            let here = off;
            off += clen;
            here
        } else {
            u64::from_le_bytes(e[8..16].try_into().unwrap()) as usize
        };
        blocks.push((blen, clen, at, out_off));
        out_off += blen;
    }
    drop(f);

    // 解压结果按块偏移直接落盘：内存里不再攒整份输出（1GB 文件曾吃掉 1.1GB 峰值 RSS）。
    let out_file = fs::File::create(outp).expect("create");
    out_file.set_len(len as u64).expect("set_len");
    let sink = Mutex::new(out_file);
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads.max(1).min(nb.max(1)) {
            sc.spawn(|| {
                let mut dec: Option<SevenZDecoder> = None;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= nb {
                        break;
                    }
                    let (blen, clen, o, at) = blocks[i];
                    let mut f = fs::File::open(inp).expect("open");
                    let packed = read_range(&mut f, o, clen).expect("read block");
                    drop(f);

                    let params = params_for(order, mem_mb, blen);
                    let out = match dec.as_mut() {
                        Some(d) => {
                            d.reset(params, Some(blen as u64)).expect("reset");
                            drain_decoder(d, &packed, blen).expect("decode")
                        }
                        None => {
                            let mut d =
                                SevenZDecoder::new(params, Some(blen as u64)).expect("decoder");
                            let out = drain_decoder(&mut d, &packed, blen).expect("decode");
                            dec = Some(d);
                            out
                        }
                    };
                    let mut w = sink.lock().unwrap();
                    w.seek(SeekFrom::Start(at as u64)).expect("seek out");
                    w.write_all(&out).expect("write block");
                }
            });
        }
    });
    sink.into_inner().unwrap().flush().expect("flush");
    len
}

pub fn files_equal(a: &str, b: &str) -> bool {
    let (mut fa, mut fb) = (fs::File::open(a).expect("a"), fs::File::open(b).expect("b"));
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        let na = fa.read(&mut ba).expect("ra");
        if na == 0 {
            return fb.read(&mut bb).expect("rb") == 0;
        }
        let mut got = 0;
        while got < na {
            let nb2 = fb.read(&mut bb[got..na]).expect("rb");
            if nb2 == 0 {
                return false;
            }
            got += nb2;
        }
        if ba[..na] != bb[..na] {
            return false;
        }
    }
}

pub fn auto_bsz(len: usize, threads: usize) -> usize {
    (len / (threads * 2).max(1)).clamp(4 << 20, 32 << 20)
}

// ---------------------------------------------------------------- FFI（给手机/桌面 App）

/// 压缩内存里的数据。成功返回 0，输出指针写进 *out_ptr / *out_len；
/// 失败返回非 0（参数非法 / 内存不足 / 编码错误）。调用方负责用 `zap_free` 释放。
///
/// 注意：这是**单块**接口，输出流不自带块头。大文件请按 4–32MB 分块各自调用，
/// 长度和解压时的 `unpacked_len` 由你自己记账（CLI 的 ZAP2 格式就是这么拼的）。
///
/// # Safety
/// `data` 必须指向至少 `len` 字节的可读内存；`out_ptr` / `out_len` 必须是可写指针。
/// 返回的缓冲区由调用方用 `zap_free(ptr, len)` 释放，且必须用返回的 `out_len`。
#[no_mangle]
pub unsafe extern "C" fn zap_compress_buf(
    data: *const u8,
    len: usize,
    order: u32,
    mem_mb: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    if data.is_null() || out_ptr.is_null() || out_len.is_null() || mem_mb == 0 {
        return 1;
    }
    let input = std::slice::from_raw_parts(data, len).to_vec();
    // 跨 FFI 边界绝不能带 panic 出去：panic 在这里会 abort 掉调用方的进程。
    let r = std::panic::catch_unwind(move || encode_block_try(&input, order.clamp(2, 16), mem_mb));
    let packed = match r {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => return 2,
        Err(_) => return 3,
    };
    let mut boxed = packed.into_boxed_slice();
    *out_len = boxed.len();
    *out_ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    0
}

/// 解压 `zap_compress_buf` 产出的缓冲区。unpacked_len 必须是该块原始长度，
/// order / mem_mb 必须与压缩时一致。成功返回 0，失败返回非 0。
///
/// # Safety
/// `data` 必须指向至少 `len` 字节的可读内存；`out_ptr` / `out_len` 必须是可写指针。
/// 返回的缓冲区由调用方用 `zap_free(ptr, len)` 释放。
#[no_mangle]
pub unsafe extern "C" fn zap_decompress_buf(
    data: *const u8,
    len: usize,
    unpacked_len: usize,
    order: u32,
    mem_mb: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    if data.is_null() || out_ptr.is_null() || out_len.is_null() || mem_mb == 0 {
        return 1;
    }
    let input = std::slice::from_raw_parts(data, len).to_vec();
    let r = std::panic::catch_unwind(move || {
        decode_block_try(&input, unpacked_len, order.clamp(2, 16), mem_mb)
    });
    let out = match r {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => return 2,
        Err(_) => return 3,
    };
    let mut boxed = out.into_boxed_slice();
    *out_len = boxed.len();
    *out_ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    0
}

/// 释放 zap_compress_buf / zap_decompress_buf 返回的缓冲区（len 必须与返回时相同）。
///
/// # Safety
/// `ptr` 只能来自本库的 `zap_compress_buf` / `zap_decompress_buf`，且 `len` 必须与
/// 当时返回的 `out_len` 完全一致；传错长度会以错误的 size 释放内存。
#[no_mangle]
pub unsafe extern "C" fn zap_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}
