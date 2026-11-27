//! 搜索规模档位基准：语料规模 × 模式类型 × 并行度。
//!
//! 运行：`cargo bench -p trit-core --bench search_scale`
//!
//! 与 `search` 基准互补：那边固定 8 MiB 语料、看各搜索路径的绝对开销；
//! 这边扫不同语料规模，确认"稀有字节预过滤"的收益随规模是否稳定，
//! 以及并行判定在多大语料上才开始摊平线程调度成本。
//!
//! 每个用例输出一行 JSON（`case` / `bytes` / `us` / `mib_per_s` / `hits` 等）。

use std::time::{Duration, Instant};

use trit_core::{MatchVerdict, Ternary, TritSearcher};

const ROUNDS: usize = 3;
const SCALES: [usize; 3] = [1, 8, 32];

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
            let line = match i % 128 {
                0 => format!("pub fn open(path: &Path) -> io::Result<File> {{ // rare needle {i}"),
                1 => format!("    let handle = std::fs::File::open(path)?; // case {i}"),
                2 => format!("// 注释：温度=三进制 {{i}}，路径={i}/data"),
                _ => format!("    let value_{i} = compute({i}); // filler line"),
            };
            bytes += line.len() + 1;
            text.push(line);
            i += 1;
        }
        Self { text, bytes }
    }

    fn joined(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes);
        for line in &self.text {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    }

    fn slices(&self) -> Vec<&[u8]> {
        self.text.iter().map(|line| line.as_bytes()).collect()
    }
}

fn measure(mut f: impl FnMut() -> usize) -> (Duration, usize) {
    std::hint::black_box(f());
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
    let jobs = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    println!("{{\"jobs\":{jobs}}}");

    for mib in SCALES {
        let corpus = Corpus::new(mib * 1024 * 1024);
        let hay = corpus.joined();
        let lines = corpus.slices();

        // 稀有字面量：预过滤收益最大的一类。
        let rare = TritSearcher::new("fn open", false).expect("rare literal");
        let (elapsed, hits) = measure(|| {
            rare.find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
                .count()
        });
        report(&format!("rare_literal/{mib}mib"), hay.len(), elapsed, hits);

        // 常见字面量：几乎每行都命中，预过滤帮助有限。
        let common = TritSearcher::new("let", false).expect("common literal");
        let (elapsed, hits) = measure(|| {
            common
                .find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
                .count()
        });
        report(
            &format!("common_literal/{mib}mib"),
            hay.len(),
            elapsed,
            hits,
        );

        // 通配：三值判定（Unknown 必须交给调用方复核）。
        let wildcard = TritSearcher::new("fn ?pen", false).expect("wildcard");
        let (elapsed, hits) = measure(|| {
            wildcard
                .find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Unknown)
                .count()
        });
        report(&format!("wildcard/{mib}mib"), hay.len(), elapsed, hits);

        // 大小写敏感与不敏感：验证不敏感路径没有退化成逐行 lower()。
        let sensitive = TritSearcher::new("File::open", true).expect("case sensitive");
        let (elapsed, hits) = measure(|| {
            sensitive
                .find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
                .count()
        });
        report(
            &format!("case_sensitive/{mib}mib"),
            hay.len(),
            elapsed,
            hits,
        );

        let insensitive = TritSearcher::new("file::open", false).expect("case insensitive");
        let (elapsed, hits) = measure(|| {
            insensitive
                .find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
                .count()
        });
        report(
            &format!("case_insensitive/{mib}mib"),
            hay.len(),
            elapsed,
            hits,
        );

        // 逐行判定：串行 vs 并行。断言两者一致，否则基准没有意义。
        let (elapsed, hits) = measure(|| {
            rare.judge_lines(&lines, 1)
                .into_iter()
                .filter(|verdict| *verdict == Ternary::True)
                .count()
        });
        report(&format!("judge_serial/{mib}mib"), hay.len(), elapsed, hits);

        let (elapsed, hits) = measure(|| {
            rare.judge_lines(&lines, jobs)
                .into_iter()
                .filter(|verdict| *verdict == Ternary::True)
                .count()
        });
        report(
            &format!("judge_parallel/{mib}mib"),
            hay.len(),
            elapsed,
            hits,
        );

        assert_eq!(
            rare.judge_lines(&lines, 1),
            rare.judge_lines(&lines, jobs),
            "parallel judgement diverged at {mib} MiB"
        );

        // Unicode 语料：多字节字符不能按字节边界随意切。
        let unicode = TritSearcher::new("温度=三进制", false).expect("unicode literal");
        let (elapsed, hits) = measure(|| {
            unicode
                .find_all(&hay, 0)
                .into_iter()
                .filter(|(_, verdict)| *verdict == MatchVerdict::Match)
                .count()
        });
        report(
            &format!("unicode_literal/{mib}mib"),
            hay.len(),
            elapsed,
            hits,
        );
    }

    println!("search_scale: ok");
}
