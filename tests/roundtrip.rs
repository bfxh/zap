//! 往返测试：覆盖空输入、单字节、可压缩文本、不可压缩随机数据（膨胀路径）、
//! 以及多块文件（分块目录 + 线程并行 + 头部字段）。

use std::io::Write;

/// 伪随机（LCG，保证可复现，不引入 rand 依赖）：这种数据 PPMd 压不动，
/// 会走进"输出比输入还大"的扩容路径。
fn lcg_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

fn text_like(n: usize) -> Vec<u8> {
    let words = [
        "wiki",
        "the",
        "of",
        "and",
        "compression",
        "entropy",
        "model",
        "context",
        "概率",
        "模型",
        "压缩",
        "无损",
        "文档",
        "字节",
    ];
    let mut out = Vec::with_capacity(n);
    let mut s: u64 = 12345;
    while out.len() < n {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        out.extend_from_slice(words[(s >> 16) as usize % words.len()].as_bytes());
        out.push(if (s >> 8).is_multiple_of(3) {
            b'\n'
        } else {
            b' '
        });
    }
    out.truncate(n);
    out
}

#[test]
fn block_roundtrip_edges() {
    for n in [0usize, 1, 3, 17, 1000] {
        let data = text_like(n);
        let packed = zap::encode_block_try(&data, 6, 16).expect("encode");
        let back = zap::decode_block_try(&packed, data.len(), 6, 16).expect("decode");
        assert_eq!(back, data, "n={n}");
    }
}

#[test]
fn block_roundtrip_text() {
    let data = text_like(512 * 1024);
    let packed = zap::encode_block_try(&data, 6, 32).expect("encode");
    assert!(
        packed.len() < data.len() / 2,
        "文本应压到一半以下，实际 {}",
        packed.len()
    );
    let back = zap::decode_block_try(&packed, data.len(), 6, 32).expect("decode");
    assert_eq!(back, data);
}

#[test]
fn block_roundtrip_incompressible() {
    // 随机数据会膨胀：早期实现按 len*1.25 固定预分配，缓冲填满时静默截断。
    // 这个用例就是钉住那条路径。
    let data = lcg_bytes(1024 * 1024, 7);
    let packed = zap::encode_block_try(&data, 6, 16).expect("encode");
    let back = zap::decode_block_try(&packed, data.len(), 6, 16).expect("decode");
    assert_eq!(back.len(), data.len());
    assert_eq!(back, data);
}

#[test]
fn file_roundtrip_multi_block() {
    let dir = std::env::temp_dir().join(format!("zap-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inp = dir.join("in.bin");
    let arc = dir.join("out.zap");
    let back = dir.join("back.bin");

    let data = text_like(5 * 1024 * 1024);
    std::fs::File::create(&inp)
        .unwrap()
        .write_all(&data)
        .unwrap();

    let bsz = 1 << 20; // 1MB → 5 块，强制走多块路径
    let (n, out_len) =
        zap::compress_file(inp.to_str().unwrap(), arc.to_str().unwrap(), 3, 6, 16, bsz);
    assert_eq!(n, data.len());
    assert_eq!(std::fs::metadata(&arc).unwrap().len() as usize, out_len);

    // 头部字段校验：magic / 块数 / 总长 / order / mem
    let head = std::fs::read(&arc).unwrap();
    assert_eq!(&head[0..4], b"ZAP2");
    assert_eq!(
        u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize,
        5
    );
    assert_eq!(
        u64::from_le_bytes(head[8..16].try_into().unwrap()) as usize,
        data.len()
    );
    assert_eq!(head[16], 6);
    assert_eq!(u16::from_le_bytes(head[17..19].try_into().unwrap()), 16);

    let got = zap::decompress_file(arc.to_str().unwrap(), back.to_str().unwrap(), 3);
    assert_eq!(got, data.len());
    assert!(zap::files_equal(
        inp.to_str().unwrap(),
        back.to_str().unwrap()
    ));
    assert!(
        out_len < data.len(),
        "可压缩输入不该变大：{} vs {}",
        out_len,
        data.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧版 ZAP1（流按块序隐式排列、目录项 8 字节）仍要能解：用户手里可能有 v0.1 的归档。
#[test]
fn decodes_v1_archive() {
    let dir = std::env::temp_dir().join(format!("zap-v1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inp = dir.join("in.bin");
    let arc = dir.join("v1.zap");
    let back = dir.join("back.bin");

    let a = text_like(64 * 1024);
    let b = text_like(48 * 1024);
    std::fs::File::create(&inp)
        .unwrap()
        .write_all(&[a.clone(), b.clone()].concat())
        .unwrap();

    let (pa, pb) = (zap::encode_block(&a, 6, 16), zap::encode_block(&b, 6, 16));
    let mut w = Vec::new();
    w.extend_from_slice(b"ZAP1");
    w.extend_from_slice(&2u32.to_le_bytes());
    w.extend_from_slice(&((a.len() + b.len()) as u64).to_le_bytes());
    w.push(6);
    w.extend_from_slice(&16u16.to_le_bytes());
    for (blen, clen) in [(a.len(), pa.len()), (b.len(), pb.len())] {
        w.extend_from_slice(&(blen as u32).to_le_bytes());
        w.extend_from_slice(&(clen as u32).to_le_bytes());
    }
    w.extend_from_slice(&pa);
    w.extend_from_slice(&pb);
    std::fs::write(&arc, &w).unwrap();

    let got = zap::decompress_file(arc.to_str().unwrap(), back.to_str().unwrap(), 2);
    assert_eq!(got, a.len() + b.len());
    assert!(zap::files_equal(
        inp.to_str().unwrap(),
        back.to_str().unwrap()
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
