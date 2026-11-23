# zap —— 能装在手机和电脑上的多线程 PPMd 压缩器

纯 Rust，零第三方运行时依赖，单文件可执行。**1GB 文件 65 秒压完**（2 线程，15.4 MB/s），
压缩率 1.6316 bit/byte；比同项目的 CM 档（1.4203）大 15%，但**快 55 倍**。

    zap c <in> <out> [threads]     压缩
    zap d <in> <out> [threads]     解压
    zap t <in> [threads]           自检（压缩 + 解压 + 逐字节比对）

不给 `threads` 就用满所有核。

## 为什么不是 CM

上下文混合（CM）能压到 1.4203 bit/byte，但单核只有 0.28 MB/s，1GB 要 59 分钟——手机上等于不能用。
CM 每字节约 100 次随机内存访存，天生重。
zap 换回 PPMd（单线程已有 9 MB/s 量级），再把输入切块并行，速度随核数上升，
代价是每块都要重新学模型。

## 旋钮

| 环境变量 | 默认 | 含义 |
|---|---|---|
| `ZAP_ORDER` | 6 | PPMd 阶数。8 更小（约 −4%），但慢 1.5 倍 |
| `ZAP_MEM` | 64 | **每线程**模型内存 MB。64 → 1.6573，256 → 1.6316 |
| `ZAP_BSZ` | 自动 | 块大小 MB。自动 = 总长 /（线程×2），夹在 4–32 |

内存是最便宜的收益：从 64MB 加到 256MB 白赚 1.6% 体积，只多花内存。
块大小则是压缩率与速度的跷跷板——块越大越省字节，并行粒度也越粗。

## v0.1 → v0.2 改了什么

两处结构性浪费，都不影响压缩率（压缩出来的**每个块逐字节不变**）：

| 项 | v0.1 | v0.2 | 变化 |
|---|---|---|---|
| 压缩 1GB（o6/32MB/256MB/2 线程） | 67.0s，14.92 MB/s | **63.6s，15.72 MB/s** | +5.4% |
| 压缩峰值 RSS | 422MB | **254MB** | −40% |
| 解压 1GB | 72.1s，13.86 MB/s | **68.6s，14.57 MB/s** | +5.2% |
| 解压峰值 RSS | 1124MB | **254MB** | −77% |
| 归档体积 | 203,952,292 | 203,952,532 | +240 字节 |

- **模型复用**：每个线程只 `new` 一次 encoder，块与块之间 `reset` 复用同一份模型存储，
  不再"压一块就申请 256MB、压完再还回去"。
- **结果直落盘**：压缩时按原子游标申请偏移、直接写进最终文件；解压同理按块偏移写。
  内存里只留索引，不再把整包压缩数据（或整份输出）攒在 `Vec` 里——解压峰值从 1.1GB 降到 254MB。
- 代价是索引从 8 字节变成 16 字节（多一个 u64 偏移），30 块多 240 字节，0.0001%。
- 码流格式随之升到 **ZAP2**；v0.1 的 ZAP1 归档仍能解码（`decodes_v1_archive` 用例 + 1GB 实测都验过）。

## 实测：enwik9（1,000,000,000 字节，本机 2 核 / 4GB）

| 配置 | 输出字节 | bit/byte | 速度 | 耗时 |
|---|---|---|---|---|
| o6, 32MB 块, 256MB, 1 线程 | 203,952,292 | 1.6316 | 7.49 MB/s | 133s |
| **o6, 32MB 块, 256MB, 2 线程** | **203,952,532** | **1.6316** | **15.72 MB/s** | **63.6s** |
| o6, 32MB 块, 64MB, 2 线程 | 207,160,058 | 1.6573 | 15.33 MB/s | 65.2s |
| o8, 32MB 块, 256MB, 2 线程 | 195,585,327 | 1.5647 | 10.44 MB/s | 95.8s |
| o6, 8MB 块, 64MB, 2 线程（手机档） | 211,917,036 | 1.6953 | 17.19 MB/s | 58.2s |

块大小的代价（16MB 切片，o6 / 256MB / 2 线程）：

| 块大小 | 输出 | bit/byte | 速度 |
|---|---|---|---|
| 16MB（整块不切） | 3,927,041 | 1.8726 | 6.79 MB/s |
| 8MB | 4,022,173 | 1.9179 | 14.58 MB/s |
| 4MB | 4,123,781 | 1.9664 | 16.35 MB/s |

解压与压缩同速同构：1GB 解码 13.75 MB/s，`cmp` 逐字节一致。

参照系（同为 enwik9）：

| 程序 | 字节 | bit/byte | 机器 |
|---|---|---|---|
| CM（本项目 lcm） | 177,537,320 | 1.4203 | 单核，59 分钟 |
| **zap o8 / 32MB / 2t** | **195,585,327** | **1.5647** | 2 核，96 秒 |
| PPMd 整块 o6 / 256MB | 197,118,058 | 1.5769 | 单核，108 秒 |
| zap o6 / 32MB / 2t | 203,952,532 | 1.6316 | 2 核，64 秒 |
| xz -9e（enwik8 口径） | — | 1.9865 | — |

## 线程不是越多越好

Windows，24 核 / 15GB，输入是一份 505MB 的已压缩安装包（8.12 bit/byte，压不动，纯测速度）：

| 线程 | 1 | 2 | 4 | 8 | 12 | 24 |
|---|---|---|---|---|---|---|
| MB/s | 4.41 | 7.54 | 12.27 | 19.03 | 22.04 | 20.11 |

拐点在 8–12 线程：PPMd 是随机访存密集的，线程再多就被内存带宽吃掉。
**电脑建议 8 线程，手机 4 线程**，不要"有多少核开多少"。解压后 SHA256 与原文件一致。

## 手机 / 电脑

`bin/` 里带预编译产物：

| 文件 | 平台 |
|---|---|
| `zap.exe` | Windows x64（cmd / PowerShell 直接跑） |
| `zap-aarch64` | Android Termux、ARM64 Linux（musl 静态链接，单文件） |
| `libzap.so` | Linux x64 动态库，给 App / 其他语言用 |

Android（Termux，免 root）：

    chmod +x zap-aarch64
    ./zap-aarch64 t /sdcard/Download/big.pdf 4

自己编：

    cargo build --release
    rustup target add aarch64-unknown-linux-musl
    RUSTFLAGS="-C linker=rust-lld" cargo build --release --target aarch64-unknown-linux-musl
    rustup target add x86_64-pc-windows-gnu
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
        cargo build --release --target x86_64-pc-windows-gnu

## C 接口（给手机 App / 别的语言）

三个函数，全部是**单块**语义，`order` 与 `mem_mb` 压解必须一致：

    int  zap_compress_buf(const uint8_t* data, size_t len, uint32_t order, uint32_t mem_mb,
                          uint8_t** out_ptr, size_t* out_len);
    int  zap_decompress_buf(const uint8_t* data, size_t len, size_t unpacked_len,
                            uint32_t order, uint32_t mem_mb,
                            uint8_t** out_ptr, size_t* out_len);
    void zap_free(uint8_t* ptr, size_t len);

返回 0 成功，1 参数非法，2 编码/解码失败，3 内部 panic（已 `catch_unwind`，不会炸调用方进程）。
输出的缓冲区用 `zap_free(ptr, len)` 释放，`len` 必须用返回值。
`test_zap.c` 是最小示例（55 字节文本 → 20 字节 → 原样还原）。

## 文件格式

ZAP2（当前）：

    "ZAP2" | nblocks u32 | total u64 | order u8 | mem_mb u16      # 19 字节
    每块：blen u32 | clen u32 | off u64                             # 16 字节 × nblocks
    各块的 PPMd 流，按 off 定位（写入顺序＝完成顺序，与块序无关）

ZAP1（v0.1，仍可解码）：目录项只有 `blen u32 | clen u32`，流按块序隐式拼接。

参数全在头部，解码只读流、不看环境变量（早期丢过参数、导致 1GB 解码在第 12 字节分叉）。
单块长度用 u32，因此块必须小于 4GB——分块设计本身已经保证了这点。

## 已知取舍

- 分块并行 = 每块重新学模型：4MB 块比整块大约 5%，8MB 约 2.5%，16MB 约 1%。
- 单块内 PPMd 仍是单线程；想再快就减块或降阶（o4 约快 1.5 倍、损 3%）。
- `ZAP_MEM` 是每线程的：4 线程 × 64MB = 256MB，低端机别开太大。
- 解压时块目录已进内存（每块 16 字节），块数据按偏移随机读、结果按偏移直写，
  既不整包载入，也不整份攒在内存里。

## 测试

    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test --release

`tests/roundtrip.rs` 覆盖空输入 / 单字节 / 可压缩文本 / 不可压缩随机数据（膨胀路径）/
多块文件（头部字段 + 并行拼装）/ 旧版 ZAP1 归档解码。随机数据那条专门钉住早期的静默截断缺陷。

## 许可

MIT OR Apache-2.0，任选其一。
