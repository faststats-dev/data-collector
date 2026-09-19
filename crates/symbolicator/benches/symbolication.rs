use std::{
    hint::black_box,
    time::{Duration, Instant},
};
use symbolicator::{JavaScriptMapping, Mapping, ProguardMapping, ReactNativeMapping, SourceMap};

const JS_MAP: &[u8] = include_bytes!("../tests/fixtures/generated/webpack/bundle.js.map");
const JS_TRACE: &str = include_str!("../tests/fixtures/generated/webpack/input.txt");
const R8_MAP: &str = include_str!("../tests/fixtures/jvm/advanced/mapping.txt");
const R8_TRACE: &str = include_str!("../tests/fixtures/jvm/advanced/input.txt");
const RN_MAP: &[u8] = include_bytes!("../tests/fixtures/generated/hermes/bundle.js.map");
const RN_TRACE: &str = include_str!("../tests/fixtures/generated/hermes/input.txt");

fn bench<T>(name: &str, bytes: usize, mut operation: impl FnMut() -> T) {
    // Calibrate rather than choosing iteration counts based on input size.
    let mut iterations = 1;
    loop {
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(operation());
        }
        if start.elapsed() >= Duration::from_millis(25) {
            break;
        }
        iterations *= 2;
    }
    let mut samples = Vec::with_capacity(7);
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(operation());
        }
        samples.push(start.elapsed().as_secs_f64() / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[3];
    println!(
        "{name:34} {:>12.0} {:>12.1}",
        median * 1e9,
        bytes as f64 / median / 1048576.0
    );
}

fn main() {
    if cfg!(debug_assertions) {
        return;
    }
    println!(
        "{:34} {:>12} {:>12}",
        "benchmark (median of 7 samples)", "ns/op", "MiB/s"
    );
    bench("parse/webpack", JS_MAP.len(), || {
        SourceMap::from_slice(black_box(JS_MAP)).unwrap()
    });
    bench("parse/metro-hermes", RN_MAP.len(), || {
        SourceMap::from_slice(black_box(RN_MAP)).unwrap()
    });
    bench("parse/r8", R8_MAP.len(), || {
        ProguardMapping::parse(black_box(R8_MAP))
    });
    let mut js = JavaScriptMapping::new();
    js.insert("bundle.js", SourceMap::from_slice(JS_MAP).unwrap());
    let jvm = ProguardMapping::parse(R8_MAP);
    let rn = ReactNativeMapping::new(SourceMap::from_slice(RN_MAP).unwrap());
    bench("apply/webpack", JS_TRACE.len(), || {
        js.apply(black_box(JS_TRACE))
    });
    bench("apply/r8-inline-outline-rewrite", R8_TRACE.len(), || {
        jvm.apply(black_box(R8_TRACE))
    });
    bench("apply/react-native-hermes", RN_TRACE.len(), || {
        rn.apply(black_box(RN_TRACE))
    });
    let large_trace = R8_TRACE.repeat(64);
    bench("apply/r8-long-trace", large_trace.len(), || {
        jvm.apply(black_box(&large_trace))
    });
    let js_long = JS_TRACE.repeat(64);
    bench("apply/js-long-trace", js_long.len(), || {
        js.apply(black_box(&js_long))
    });
    let missing = "    at run (missing.js:1:1)\n".repeat(1024);
    bench("apply/js-missing-map", missing.len(), || {
        js.apply(black_box(&missing))
    });
    let unmapped = "not a stack frame\n".repeat(4096);
    bench("apply/js-unmapped-68k", unmapped.len(), || {
        js.apply(black_box(&unmapped))
    });
    bench("apply/jvm-unmapped-68k", unmapped.len(), || {
        jvm.apply(black_box(&unmapped))
    });
    for count in [10, 1_000, 10_000] {
        let mut text = String::from("example.Large -> a:\n");
        for index in 0..count {
            text.push_str(&format!(
                "    {index}:{index}:void method{index}():42:42 -> m\n"
            ));
        }
        if count == 10_000 {
            bench("parse/jvm-10000-ranges", text.len(), || {
                ProguardMapping::parse(black_box(&text))
            });
        }
        let map = ProguardMapping::parse(&text);
        let trace = format!("\tat a.m(SourceFile:{})\n", count - 1);
        bench(&format!("apply/jvm-{count}-ranges"), trace.len(), || {
            map.apply(black_box(&trace))
        });
    }
    // Measures class/method indexing independently of unrelated mapping size.
    for count in [10, 1_000, 10_000] {
        let mut text = String::from("example.Large -> a:\n");
        for index in 0..count {
            text.push_str(&format!("    1:1:void method{index}():42:42 -> m{index}\n"));
        }
        let map = ProguardMapping::parse(&text);
        let trace = format!("\tat a.m{}(SourceFile:1)\n", count - 1);
        bench(&format!("apply/jvm-{count}-methods"), trace.len(), || {
            map.apply(black_box(&trace))
        });
        if count == 10_000 {
            bench("parse/jvm-10000-methods", text.len(), || {
                ProguardMapping::parse(black_box(&text))
            });
        }
    }
    // Repeated ranges of one original method, with line information missing.
    let mut text = String::from("example.Large -> a:\n");
    for index in 0..10_000 {
        text.push_str(&format!("    {index}:{index}:void run():42:42 -> m\n"));
    }
    let map = ProguardMapping::parse(&text);
    let trace = "\tat a.m(Unknown Source)\n";
    bench("apply/jvm-no-line-10000-ranges", trace.len(), || {
        map.apply(black_box(trace))
    });

    for count in [10, 1_000] {
        let sections: Vec<_> = (0..count)
            .map(|line| {
                serde_json::json!({
                    "offset": {"line": line, "column": 0},
                    "map": {"version": 3, "sources": ["app.ts"], "names": [], "mappings": "AAAA"}
                })
            })
            .collect();
        let bytes =
            serde_json::to_vec(&serde_json::json!({"version": 3, "sections": sections})).unwrap();
        let mut maps = JavaScriptMapping::new();
        maps.insert("bundle.js", SourceMap::from_slice(&bytes).unwrap());
        let trace = format!("    at bundle.js:{count}:1\n");
        bench(&format!("apply/js-{count}-sections"), trace.len(), || {
            maps.apply(black_box(&trace))
        });
    }
    let sources: Vec<_> = (0..1_000).map(|i| format!("src/module{i}.ts")).collect();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 3, "sources": sources, "names": [], "mappings": "AAAA"
    }))
    .unwrap();
    bench("parse/js-1000-sources", bytes.len(), || {
        SourceMap::from_slice(black_box(&bytes)).unwrap()
    });
}
