//! 搜索实现的随机对拍：`TritSearcher`（稀有字节预过滤）vs `naive_find`（穷举 oracle）。
//!
//! 预过滤是性能优化，绝不能改变判定。这里用确定性 LCG 生成随机语料与模式
//! （含 `?` 通配、大小写两种模式），逐个位置与 oracle 对比偏移和判定；
//! 任何不一致都说明预过滤漏掉了某种字节组合。
//!
//! 另一个测试固定"并行逐行判定 == 串行逐行判定"，防止分块逻辑引入顺序或边界差异。

use trit_core::search::naive_find;
use trit_core::{MatchVerdict, Ternary, TritSearcher};

/// 确定性线性同余生成器：失败可复现，不引入随机种子依赖。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

const ALPHABET: &[u8] = b"abcdeABCDE_ ./\\0123\n\t:;(){}[]<>=+\"'";

fn random_text(rng: &mut Lcg, len: usize) -> Vec<u8> {
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

fn random_pattern(rng: &mut Lcg, max_len: usize, wildcards: bool) -> String {
    let len = 1 + rng.below(max_len);
    (0..len)
        .map(|_| {
            if wildcards && rng.below(4) == 0 {
                '?'
            } else {
                ALPHABET[rng.below(ALPHABET.len())] as char
            }
        })
        .collect()
}

/// oracle：与 `find_all` 相同的非重叠推进规则，但每个位置都用穷举比较。
fn oracle_find_all(
    hay: &[u8],
    pattern: &str,
    case_insensitive: bool,
) -> Vec<(usize, MatchVerdict)> {
    let mut out = Vec::new();
    let mut base = 0usize;
    let mut rest = hay;
    while let Some((off, verdict)) = naive_find(pattern, case_insensitive, rest) {
        out.push((base + off, verdict));
        // 与实现一致：匹配起点后前进一个字节，允许重叠起点。
        base += off + 1;
        rest = &rest[off + 1..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

#[test]
fn find_all_matches_naive_oracle_on_random_corpora() {
    let mut rng = Lcg::new(0x5eed_1234);
    for round in 0..300 {
        let len = 1 + rng.below(256);
        let hay = random_text(&mut rng, len);
        let wildcards = round % 3 == 0;
        let pattern = random_pattern(&mut rng, 6, wildcards);
        for case_insensitive in [false, true] {
            let searcher = TritSearcher::new(&pattern, case_insensitive)
                .unwrap_or_else(|error| panic!("round {round} pattern {pattern:?}: {error}"));
            let actual = searcher.find_all(&hay, 0);
            let expected = oracle_find_all(&hay, &pattern, case_insensitive);
            assert_eq!(
                actual,
                expected,
                "round {round}: pattern {pattern:?} hay {:?} ci={case_insensitive}",
                String::from_utf8_lossy(&hay)
            );
        }
    }
}

#[test]
fn find_all_respects_limit_without_changing_prefix() {
    let mut rng = Lcg::new(0xabc_def);
    let hay = random_text(&mut rng, 512);
    let searcher = TritSearcher::new("a", false).expect("single byte pattern");
    let all = searcher.find_all(&hay, 0);
    if all.len() < 2 {
        return;
    }
    for limit in [1usize, 2, all.len() / 2, all.len()] {
        let limited = searcher.find_all(&hay, limit);
        assert_eq!(limited.len(), limit.min(all.len()));
        assert_eq!(&limited[..], &all[..limited.len()], "limit {limit}");
    }
}

#[test]
fn parallel_judgement_matches_serial() {
    let mut rng = Lcg::new(0x1234_5678);
    let lines: Vec<Vec<u8>> = (0..257)
        .map(|_| {
            let len = rng.below(40);
            random_text(&mut rng, len)
        })
        .collect();
    let slices: Vec<&[u8]> = lines.iter().map(Vec::as_slice).collect();

    for pattern in ["a", "ab", "a?c", "??", "e_", "A"] {
        for case_insensitive in [false, true] {
            let searcher = TritSearcher::new(pattern, case_insensitive).unwrap();
            let serial = searcher.judge_lines(&slices, 1);
            for jobs in [2usize, 3, 8, 16] {
                let parallel = searcher.judge_lines(&slices, jobs);
                assert_eq!(
                    serial, parallel,
                    "pattern {pattern:?} ci={case_insensitive} jobs={jobs}"
                );
            }
            // 逐行的三值判定必须与单行接口一致。
            for (line, verdict) in lines.iter().zip(&serial) {
                assert_eq!(*verdict, searcher.line_verdict(line));
            }
        }
    }
}

#[test]
fn unicode_corpus_never_panics_and_matches_byte_oracle() {
    let hay = "温度=三进制温 温度x温?度".as_bytes();
    for pattern in ["温度", "三进制", "温?度", "?", "不存在"] {
        for case_insensitive in [false, true] {
            let searcher = TritSearcher::new(pattern, case_insensitive).unwrap();
            let actual = searcher.find_all(hay, 0);
            let expected = oracle_find_all(hay, pattern, case_insensitive);
            assert_eq!(actual, expected, "pattern {pattern:?}");
        }
    }
}

#[test]
fn empty_and_oversized_patterns_are_not_matches() {
    // 空模式不匹配任何位置（否则每个偏移都会命中）。
    for pattern in ["", "abcdefghij"] {
        if let Ok(searcher) = TritSearcher::new(pattern, false) {
            assert!(searcher.find_all(b"short", 0).is_empty(), "{pattern:?}");
        }
    }
    let searcher = TritSearcher::new("a", false).unwrap();
    assert!(searcher.find_all(b"", 0).is_empty());
    assert_eq!(searcher.line_verdict(b""), Ternary::False);
}
