//! 单次调用耗时基线：`encode_into` / `encode`（拥有值）/ `decode_from`（raw 与 packed）。
//!
//! 只出数据、不设通过门。与 `_recon/harness/bench` 的 C# `FastSerialize` /
//! `FastDeserialize` 使用同一组代表值、同一热身/轮次/迭代数；计时只包住调用本身，
//! 进程启动、值构造与编码结果长度探测都在计时之外。`dst` 按本次编码长度预分配，
//! `scratch` 固定 64 KiB；解码目标实例跨轮复用。
//!
//! `encode` 行（op 名 `encode_owned`）与 `encode_into` 行并列，衡量拥有值返回形态：
//! 计时**包含**每次调用的 `Vec` 分配/增长与 `finish` 里的最终拷贝，也包含结果 `Vec` 的释放；
//! 输入用 `black_box(value)`，输出用 `black_box(encoded.as_slice())`，避免调用被优化掉。
//! 每次调用的堆分配次数由本 example 内的计数分配器（`CountingAllocator`）统计：
//! 独立一轮循环读取计数器增量再除以调用次数；该分配器对全进程生效，每次分配只多一条
//! `Relaxed` 原子自增，对计时的影响在噪声量级。
//!
//! 表格第 1 段是逐操作行，第 2 段（表头 `value,mode,encode_into_ns/op,...`）给出同值同模式下
//! 两种形态的 ns/op、比值 `encode_owned / encode_into` 与 `encode_owned` 的每次调用分配次数。
//!
//! 运行：`cargo run --release --example bench -p ace-serialize --features std`

use ace_serialize::limits::Options;
use ace_serialize::schema::{AceDecode, AceEncode};
use ace_serialize::{decode_from, encode, encode_into};
use ace_serialize_derive::AceSerialize;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// 与两侧约定一致：每项先热身 1000 次，测量 5 轮，取轮内 `ns/op` 的中位数。
const WARMUP_ITERS: usize = 10_000;
const ROUNDS: usize = 5;
/// `encode_into` 的调用方工作区，固定值（C# 侧 `FastSerialize` 使用库内默认 writer）。
const SCRATCH_BYTES: usize = 64 * 1024;
/// 分配计数轮的调用次数：每次 `encode` 的分配次数恒定，取 1000 次足以摊平。
const ALLOC_COUNT_ITERS: usize = 1_000;

/// 计数分配器：只用于统计 `encode`（拥有值）每次调用的堆分配次数。
///
/// 进程单线程运行，`Relaxed` 自增只保证计数不丢；`realloc` 按一次分配计（`Vec<u8>` 的增长走它）。
struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

/// `c.struct`：3 个 `i32` 字段，按声明序。
#[derive(AceSerialize)]
struct ThreeInts {
    x: i32,
    y: i32,
    z: i32,
}

/// `g.shared` 的叶子对象。
#[derive(AceSerialize)]
struct OneInt {
    value: i32,
}

/// `g.shared`：两个字段指向同一个 `Rc`。
#[derive(AceSerialize)]
struct SharedPair {
    left: Rc<OneInt>,
    right: Rc<OneInt>,
}

fn options(packed: bool) -> Options {
    Options {
        packed,
        ..Options::default()
    }
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("计时样本不得为 NaN"));
    samples[samples.len() / 2]
}

/// 每个代表值的解码目标初值（与编码值同形，跨轮复用）。
trait Sample: Sized {
    fn sample() -> Self;
}

impl Sample for i32 {
    fn sample() -> Self {
        0
    }
}

impl Sample for ThreeInts {
    fn sample() -> Self {
        Self { x: 0, y: 0, z: 0 }
    }
}

impl Sample for Vec<i32> {
    fn sample() -> Self {
        Vec::with_capacity(200)
    }
}

impl Sample for String {
    fn sample() -> Self {
        String::with_capacity(16)
    }
}

impl Sample for SharedPair {
    fn sample() -> Self {
        let child = Rc::new(OneInt { value: 0 });
        Self {
            left: Rc::clone(&child),
            right: child,
        }
    }
}

/// 编码：`dst` 与 `scratch` 在计时外分配并跨轮复用，返回 `(ns/op 中位数, 编码字节数)`。
fn bench_encode<T: AceEncode>(value: &T, packed: bool, iters: usize) -> (f64, usize) {
    let options = options(packed);
    let encoded_len = encode(value, &options)
        .expect("长度探测必须成功（只用于预分配 dst）")
        .len();
    let mut dst = vec![0u8; encoded_len];
    let mut scratch = vec![0u8; SCRATCH_BYTES];

    for _ in 0..WARMUP_ITERS {
        let written = encode_into(value, &mut dst, &mut scratch, &options).expect("热身必须成功");
        assert_eq!(written, encoded_len);
    }

    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..iters {
            let written = encode_into(
                black_box(value),
                black_box(dst.as_mut_slice()),
                black_box(scratch.as_mut_slice()),
                &options,
            )
            .expect("测量必须成功");
            black_box(written);
        }
        samples.push(start.elapsed().as_nanos() as f64 / iters as f64);
    }
    (median(samples), encoded_len)
}

/// 拥有值编码：每次调用新分配结果 `Vec`，返回 `(ns/op 中位数, 每次调用的堆分配次数)`。
fn bench_encode_owned<T: AceEncode>(value: &T, packed: bool, iters: usize) -> (f64, usize) {
    let options = options(packed);

    for _ in 0..WARMUP_ITERS {
        let encoded = encode(value, &options).expect("热身必须成功");
        black_box(encoded.as_slice());
    }

    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..iters {
            let encoded = encode(black_box(value), &options).expect("测量必须成功");
            black_box(encoded.as_slice());
        }
        samples.push(start.elapsed().as_nanos() as f64 / iters as f64);
    }

    // 分配次数与时长无关，单独一轮计数；循环内除 `encode` 自身外不发生其它分配。
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..ALLOC_COUNT_ITERS {
        let encoded = encode(value, &options).expect("分配计数必须成功");
        black_box(encoded.as_slice());
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocations % ALLOC_COUNT_ITERS,
        0,
        "每次调用的分配次数必须恒定"
    );
    (median(samples), allocations / ALLOC_COUNT_ITERS)
}

/// 解码：`src` 由计时外的 `encode` 生成，目标实例跨轮复用，返回 `ns/op` 中位数。
fn bench_decode<T: AceDecode>(src: &[u8], out: &mut T, packed: bool, iters: usize) -> f64 {
    let options = options(packed);

    for _ in 0..WARMUP_ITERS {
        decode_from(src, out, &options).expect("热身必须成功");
    }

    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..iters {
            decode_from(black_box(src), black_box(&mut *out), &options).expect("测量必须成功");
        }
        samples.push(start.elapsed().as_nanos() as f64 / iters as f64);
    }
    // 目标实例跨轮复用；测量后读一次，使解码结果保持可观察。
    black_box(&*out);
    median(samples)
}

/// 同一值/模式下 `encode_into` 与 `encode`（拥有值）的对照行。
struct Comparison {
    value: &'static str,
    mode: &'static str,
    encode_into_ns: f64,
    encode_owned_ns: f64,
    encode_owned_allocations: usize,
}

fn print_row(value: &str, op: &str, mode: &str, ns_per_op: f64, iters: usize) {
    println!("{value},{op},{mode},{ns_per_op:.2},{iters}");
}

fn run_value<T>(name: &'static str, value: &T, iters: usize) -> Vec<Comparison>
where
    T: AceEncode + AceDecode + Sample,
{
    let mut comparisons = Vec::with_capacity(2);
    for (mode, packed) in [("raw", false), ("packed", true)] {
        let (encode_ns, encoded_len) = bench_encode(value, packed, iters);
        print_row(name, "encode", mode, encode_ns, iters);

        let (encode_owned_ns, encode_owned_allocations) = bench_encode_owned(value, packed, iters);
        print_row(name, "encode_owned", mode, encode_owned_ns, iters);
        comparisons.push(Comparison {
            value: name,
            mode,
            encode_into_ns: encode_ns,
            encode_owned_ns,
            encode_owned_allocations,
        });

        let src = encode(value, &options(packed)).expect("解码样本必须成功生成");
        assert_eq!(src.len(), encoded_len);
        let mut out = T::sample();
        let decode_ns = bench_decode(&src, &mut out, packed, iters);
        print_row(name, "decode", mode, decode_ns, iters);
    }
    comparisons
}

fn rustc_version() -> String {
    let output = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .expect("rustc --version 必须可执行");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn main() {
    println!(
        "env,os={},arch={},rustc=\"{}\",warmup_iters={WARMUP_ITERS},rounds={ROUNDS},scratch_bytes={SCRATCH_BYTES}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        rustc_version(),
    );
    println!("value,op,mode,ns/op,iters");

    let int_value: i32 = 0x1234_5678;
    let struct_value = ThreeInts {
        x: 0x1234_5678,
        y: -2,
        z: 7,
    };
    let array_value: Vec<i32> = (0..200).collect();
    let string_value = String::from("中文");
    let shared_child = Rc::new(OneInt { value: 7 });
    let shared_value = SharedPair {
        left: Rc::clone(&shared_child),
        right: shared_child,
    };

    let mut comparisons = Vec::new();
    comparisons.extend(run_value("a.int", &int_value, 50_000));
    comparisons.extend(run_value("c.struct", &struct_value, 50_000));
    comparisons.extend(run_value("f.array200", &array_value, 2_000));
    comparisons.extend(run_value("b.chinese", &string_value, 20_000));
    comparisons.extend(run_value("g.shared", &shared_value, 20_000));

    println!();
    println!("value,mode,encode_into_ns/op,encode_owned_ns/op,ratio,encode_owned_allocs/op");
    for comparison in &comparisons {
        println!(
            "{},{},{:.2},{:.2},{:.2},{}",
            comparison.value,
            comparison.mode,
            comparison.encode_into_ns,
            comparison.encode_owned_ns,
            comparison.encode_owned_ns / comparison.encode_into_ns,
            comparison.encode_owned_allocations,
        );
    }
}
