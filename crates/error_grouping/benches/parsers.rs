use std::{hint::black_box, time::Instant};

use error_grouping::{fingerprint, parse, parser};

const JAVA: &str = "java.lang.RuntimeException: boom\n    at app.Main.run(Main.java:42)\nCaused by: java.lang.IllegalStateException: bad\n    at app.Work.go(Work.java:7)\n    ... 2 more";
const JAVA_NESTED: &str = "java.lang.Error: root\n    at loader/java.base@17/java.lang.Thread.run(Thread.java:1)\n    Suppressed: java.lang.IllegalStateException: suppressed\n        at app.Work.close(Work.java:8)\n        Caused by: java.io.IOException: nested\n            at app.IO.fail(IO.java:9)\nCaused by: java.lang.RuntimeException: cause\n    at app.Main.run(Main.java:42)";
const RUST: &str = "thread 'main' panicked at src/main.rs:12:5:\nindex out of bounds\nstack backtrace:\n   0: 0xabc - demo::run\n             at ./src/main.rs:12:5\n   1: std::rt::lang_start";
const JAVASCRIPT: &str = "TypeError: nope\n    at async run (/srv/app.js:10:7)\n    at new Worker (node:internal/workers:22:3)\n    at nativeCall (native)";
const PYTHON: &str = "Traceback (most recent call last):\n  File \"/app.py\", line 3, in load\n    int('x')\nValueError: invalid";
const PHP: &str = "PHP Fatal error: Uncaught TypeError: bad in /app/index.php:12\nStack trace:\n#0 /app/index.php(8): App\\Worker->run()\n#1 {main}";
const GO: &str = "panic: send on closed channel\n\ngoroutine 18 [running]:\nmain.worker(0x1, 0x2)\n\t/work/main.go:14 +0x4f\ncreated by main.main in goroutine 1\n\t/work/main.go:8 +0x20";
const SWIFT: &str = "Swift/ErrorType.swift:254: Fatal error: Error raised at top level\n\nProgram crashed: System trap at 0x1\n\nThread 0 crashed:\n0 0x1 _assertionFailure(_:_:file:line:flags:) + 176 in libswiftCore.dylib\n1 0x2 App.run() + 41 in demo at /work/Sources/demo/main.swift:35:11";

fn main() {
    if cfg!(debug_assertions) {
        return;
    }

    println!("benchmark                         ns/trace       MiB/s");
    bench("java/direct", JAVA, || parser::java::parse(black_box(JAVA)));
    bench("java/detect", JAVA, || parse(black_box(JAVA)));
    bench("java/nested-direct", JAVA_NESTED, || {
        parser::java::parse(black_box(JAVA_NESTED))
    });
    bench("javascript/direct", JAVASCRIPT, || {
        parser::javascript::parse(black_box(JAVASCRIPT))
    });
    let fingerprint_trace = parser::java::parse(JAVA_NESTED).unwrap();
    bench("fingerprint/java-nested", JAVA_NESTED, || {
        fingerprint(black_box(&fingerprint_trace))
    });
    bench("group/java-nested", JAVA_NESTED, || {
        let trace = parser::java::parse(black_box(JAVA_NESTED)).unwrap();
        fingerprint(black_box(&trace))
    });
    bench_mixed();

    let large_java = large_java_trace(512);
    bench("java/512-frames", &large_java, || {
        parser::java::parse(black_box(&large_java))
    });

    let noisy_java = noisy_java_trace(128);
    bench("java/noise-discarded", &noisy_java, || {
        parser::java::parse(black_box(&noisy_java))
    });

    let line = "not a stack frame\n";
    let malformed = line.repeat((64 * 1024) / line.len());
    bench("unknown/64-kib", &malformed, || {
        parse(black_box(&malformed))
    });
}

fn bench<F, T>(name: &str, input: &str, mut operation: F)
where
    F: FnMut() -> T,
{
    let iterations = ((64 * 1024 * 1024) / input.len()).clamp(2_000, 1_000_000);
    for _ in 0..iterations / 20 {
        let _ = black_box(operation());
    }
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = black_box(operation());
    }
    report(name, input.len() * iterations, iterations, start.elapsed());
}

fn bench_mixed() {
    const CASES: [&str; 7] = [JAVA, RUST, JAVASCRIPT, PYTHON, PHP, GO, SWIFT];
    const ITERATIONS: usize = 500_000;
    for index in 0..ITERATIONS / 20 {
        let _ = black_box(parse(black_box(CASES[index % CASES.len()])));
    }
    let start = Instant::now();
    let mut bytes = 0;
    for index in 0..ITERATIONS {
        let input = CASES[index % CASES.len()];
        bytes += input.len();
        let _ = black_box(parse(black_box(input)));
    }
    report("all/detect-mixed", bytes, ITERATIONS, start.elapsed());
}

fn report(name: &str, bytes: usize, iterations: usize, elapsed: std::time::Duration) {
    let ns = elapsed.as_nanos() / iterations as u128;
    #[expect(clippy::cast_precision_loss, reason = "throughput is approximate")]
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{name:32} {ns:>8} {throughput:>12.1}",
        throughput = mib / elapsed.as_secs_f64()
    );
}

fn large_java_trace(frames: usize) -> String {
    let mut trace = String::from("java.lang.RuntimeException: boom\n");
    for index in 0..frames {
        trace.push_str("    at app.service.Worker.run(Worker.java:");
        trace.push_str(&(index + 1).to_string());
        trace.push_str(")\n");
    }
    trace
}

fn noisy_java_trace(lines: usize) -> String {
    let mut trace = String::from("java.lang.RuntimeException: boom\n");
    for _ in 0..lines {
        trace.push_str("diagnostic text that is not part of the trace\n");
    }
    trace.push_str("    at app.Main.run(Main.java:42)\n");
    trace
}
