//! Separate from timing benchmarks so allocation instrumentation cannot skew timings.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};
use symbolicator::{JavaScriptMapping, Mapping, ProguardMapping, SourceMap};
struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Relaxed);
            ALLOCS.fetch_add(1, Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, old, size) };
        if !ptr.is_null() {
            LIVE.fetch_add(size, Relaxed);
            LIVE.fetch_sub(old.size(), Relaxed);
            ALLOCS.fetch_add(1, Relaxed);
        }
        ptr
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn measure<T>(name: &str, parse: impl FnOnce() -> T) {
    let before = LIVE.load(Relaxed);
    let allocs = ALLOCS.load(Relaxed);
    let map = parse();
    let retained = LIVE.load(Relaxed) - before;
    let allocations = ALLOCS.load(Relaxed) - allocs;
    println!("{name:30} {retained:>12} {allocations:>12}");
    std::hint::black_box(map);
}
fn main() {
    if cfg!(debug_assertions) {
        return;
    }
    println!("{:30} {:>12} {:>12}", "parser", "retained B", "allocations");
    let js = include_bytes!("../tests/fixtures/generated/hermes/bundle.js.map");
    measure("hermes/upstream decoder", || {
        sourcemap::decode_slice(js).unwrap()
    });
    measure("hermes/symbolicator", || SourceMap::from_slice(js).unwrap());
    let mut jvm = String::from("example.Large -> a:\n");
    for i in 0..10_000 {
        jvm.push_str(&format!("    {i}:{i}:void method{i}():42:42 -> m\n"));
    }
    measure("jvm/10000-ranges", || ProguardMapping::parse(&jvm));
    // Production maps often embed source content much larger than their mappings.
    let large = format!(
        r#"{{"version":3,"sources":["app.ts"],"sourcesContent":["{}"],"names":[],"mappings":"AAAA"}}"#,
        "x".repeat(1024 * 1024)
    );
    measure("1MiB-source/upstream decoder", || {
        sourcemap::decode_slice(large.as_bytes()).unwrap()
    });
    measure("1MiB-source/symbolicator", || {
        SourceMap::from_slice(large.as_bytes()).unwrap()
    });
    let sources: Vec<_> = (0..1_000).map(|i| format!("src/module{i}.ts")).collect();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 3, "sources": sources, "names": [], "mappings": "AAAA"
    }))
    .unwrap();
    measure("1000-sources/upstream decoder", || {
        sourcemap::decode_slice(&bytes).unwrap()
    });
    measure("1000-sources/symbolicator", || {
        SourceMap::from_slice(&bytes).unwrap()
    });
    let trace = "not a stack frame\n".repeat(4096);
    let js = JavaScriptMapping::new();
    let jvm = ProguardMapping::parse(&jvm);
    measure("unmapped/javascript", || js.apply(&trace));
    measure("unmapped/jvm", || jvm.apply(&trace));
}
