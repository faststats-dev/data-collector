use std::{hint::black_box, time::Instant};

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 Chrome/121.0 Mobile Safari/537.36 SamsungBrowser/25.0",
    "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0",
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
    "curl/8.7.1",
];

fn main() {
    user_agent::init();

    // Long enough to smooth out scheduler noise without needing a benchmark dependency.
    let iterations = 50_000usize;
    let mut samples = Vec::with_capacity(7);
    for _ in 0..7 {
        let started = Instant::now();
        for _ in 0..iterations {
            for user_agent in USER_AGENTS {
                black_box(user_agent::parse(black_box(user_agent)));
            }
        }
        samples.push(started.elapsed().as_nanos() / (iterations * USER_AGENTS.len()) as u128);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];

    println!(
        "user-agent parse: {median} ns/parse median (range {}..{}; {} total parses)",
        samples[0],
        samples[samples.len() - 1],
        iterations * USER_AGENTS.len() * samples.len(),
    );
}
