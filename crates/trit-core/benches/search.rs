//! 无第三方依赖的基准：稀有字节预过滤 + Kleene 三值验证 + 并行批量判定。
//!
//! 运行：
//!
//! ```text
//! cargo bench -p trit-core --bench search
//! ```
//!
//! 每个用例输出一行 JSON（`case` / `bytes` / `us` / `mib_per_s` / `hits`），
//! 便于后续与 `search_code`、ripgrep 在同一语料上对比；末行输出 `bench: ok`。
//!
//! 计时取多轮最小值，避免调度抖动；每轮把命中数作为校验和返回，防止优化器删除工作。

use std::time::{Duration, Instant};

use trit_core::search::naive_find;
use trit_core::{MatchVerdict, TritSearcher};

const TARGET_BYTES: usize = 8 * 1024 * 1024;
const ROUNDS: usize = 5;
const WARMUP: usize = 1;

struct Corpus {
    text: Vec<String>,
    bytes: usize,
}

impl Corpus {
    fn new(target: usize) -> Self {
        let mut text = Vec::new();
        let mut bytes = 0usize;
        let mut i = 0usize;
        while bytes < target {
            let line = match i % 64 {
                0 => format!("pub fn open(path: &Path) -> io::Result<File> {{ // case {i}"),
                1 => format!("    let handle = File::open(path)?; // needle {i}"),
                _ => format!("    let value_{i} = compute({i}); // filler line"),
            };
            bytes += line.len() + 1;
            text.push(line);
            i += 1;
        }
        Self { text, bytes }
    }

    fn slices(&self) -> Vec<&[u8]> {
        self.text.iter().map(|line| line.as_bytes()).collect()
    }

    fn joined(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes);
        for line in &self.text {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    }
}

fn measure(mut f: impl FnMut() -> usize) -> (Duration, usize) {
    for _ in 0..WARMUP {
        std::hint::black_box(f());
    }
    let mut best = Duration::MAX;
    let mut checksum = 0usize;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        checksum = std::hint::black_box(f());
        best = best.min(started.elapsed());
    }
    (best, checksum)
}

fn report(case: &str, bytes: usize, elapsed: Duration, hits: usize) {
    let us = elapsed.as_micros().max(1);
    let mib_per_s = bytes as f64 / (1024.0 * 1024.0) / (us as f64 / 1_000_000.0);
    println!(
        "{{\"case\":\"{case}\",\"bytes\":{bytes},\"us\":{us},\"mib_per_s\":{mib_per_s:.2},\"hits\":{hits}}}"
    );
}

fn main() {
    let corpus = Corpus::new(TARGET_BYTES);
    let hay = corpus.joined();
    let lines = corpus.slices();
    let jobs = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    println!(
        "{{\"corpus_bytes\":{},\"lines\":{},\"jobs\":{jobs}}}",
        corpus.bytes,
        lines.len()
    );

    let literal = TritSearcher::new("fn open", false).expect("literal pattern");
    let (elapsed, hits) = measure(|| {
        literal
            .find_all(&hay, 0)
            .into_iter()
            .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
            .count()
    });
    report("find_all_literal", hay.len(), elapsed, hits);

    let wildcard = TritSearcher::new("fn ?pen", false).expect("wildcard pattern");
    let (elapsed, hits) = measure(|| {
        wildcard
            .find_all(&hay, 0)
            .into_iter()
            .filter(|(_, verdict)| *verdict == MatchVerdict::Unknown)
            .count()
    });
    report("find_all_wildcard", hay.len(), elapsed, hits);

    let all_wild = TritSearcher::new("????", false).expect("all-wildcard pattern");
    let (elapsed, hits) = measure(|| all_wild.find_all(&hay, 0).len());
    report("find_all_all_wildcard", hay.len(), elapsed, hits);

    let (elapsed, hits) = measure(|| {
        literal
            .judge_lines(&lines, 1)
            .into_iter()
            .filter(|verdict| *verdict == trit_core::Ternary::True)
            .count()
    });
    report("judge_lines_serial", hay.len(), elapsed, hits);

    let (elapsed, hits) = measure(|| {
        literal
            .judge_lines(&lines, jobs)
            .into_iter()
            .filter(|verdict| *verdict == trit_core::Ternary::True)
            .count()
    });
    report("judge_lines_parallel", hay.len(), elapsed, hits);

    let (elapsed, hits) = measure(|| {
        lines
            .iter()
            .filter(|line| {
                matches!(
                    naive_find("fn open", false, line),
                    Some((_, MatchVerdict::Match))
                )
            })
            .count()
    });
    report("naive_oracle", hay.len(), elapsed, hits);

    // 并行与串行必须给出完全相同的判定，否则基准没有意义。
    let serial = literal.judge_lines(&lines, 1);
    let parallel = literal.judge_lines(&lines, jobs);
    assert_eq!(serial, parallel, "parallel judgement diverged");
    println!("bench: ok");
}
