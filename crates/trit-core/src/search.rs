//! 三进制文本搜索（稀有字节预过滤 + Kleene 三值窗口验证 + 并行批量判定）。
//!
//! 模式语法：字面量字节 + `?`（单字节通配，三值语义中的「未知」）。
//!
//! 搜索分两步：
//!
//! 1. **预过滤**：取模式中最稀有的确定字节 `r`（偏移 `k`），用 `iter().position` 跳到下一个 `r`；
//!    任何匹配窗口都要求 `hay[s + k] == r`，因此窗口起点只能是 `r` 的出现位置减 `k`。
//! 2. **验证**：对候选窗口逐位置做 Kleene 三值比较——任一确定不等 → 排除；
//!    全部确定相等 → [`MatchVerdict::Match`]；否则 → [`MatchVerdict::Unknown`]。
//!
//! `judge_lines` 用 [`crate::parallel::map`] 并行判定，结果顺序与串行一致。
//!
//! 无通配符且大小写敏感时走 `memchr::memmem` 的子串快路径；含通配符时用
//! `memchr`/`memchr2` 跳稀有字节，避免逐字节 `position` 扫描。

use memchr::{memchr, memchr2, memmem};

use crate::parallel;
use crate::value::Ternary;

/// 单次搜索的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchVerdict {
    /// 所有位置都确定匹配。
    Match,
    /// 存在确定不匹配。
    NoMatch,
    /// 没有确定不匹配，但含有未知（通配）位置。
    Unknown,
}

/// 编译后的三进制搜索器。
#[derive(Debug, Clone)]
pub struct TritSearcher {
    /// 模式各位置：`None` 为通配（未知位），`Some(c)` 为已折叠的确定字节。
    pattern: Vec<Option<u8>>,
    /// 模式长度（字节数）。
    len: usize,
    /// 预过滤字节（模式中最稀有的确定字节，已折叠）；全部为通配时为 `None`。
    rare: Option<u8>,
    /// 预过滤字节在模式中的偏移。
    rare_at: usize,
    case_insensitive: bool,
    /// 预编译的子串查找器；无通配且大小写敏感时存在。
    finder: Option<memmem::Finder<'static>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern too long: {0} > 4096")]
    TooLong(usize),
}

impl TritSearcher {
    /// 编译模式。`?` 为单字节通配（Kleene 未知位）。
    pub fn new(pattern: &str, case_insensitive: bool) -> Result<Self, SearchError> {
        let bytes = pattern.as_bytes();
        if bytes.is_empty() {
            return Err(SearchError::Empty);
        }
        if bytes.len() > 4096 {
            return Err(SearchError::TooLong(bytes.len()));
        }
        let mut pattern = Vec::with_capacity(bytes.len());
        let mut rare = None;
        let mut rare_at = 0;
        let mut rare_score = u8::MAX;
        for (j, &b) in bytes.iter().enumerate() {
            if b == b'?' {
                pattern.push(None);
                continue;
            }
            let folded = if case_insensitive {
                b.to_ascii_lowercase()
            } else {
                b
            };
            pattern.push(Some(folded));
            let score = rarity(folded);
            if score < rare_score {
                rare_score = score;
                rare = Some(folded);
                rare_at = j;
            }
        }
        let finder = (!case_insensitive && !pattern.iter().any(Option::is_none))
            .then(|| memmem::Finder::new(bytes).into_owned());
        Ok(Self {
            pattern,
            len: bytes.len(),
            rare,
            rare_at,
            case_insensitive,
            finder,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// `TritSearcher` 不接受空模式，因此永远非空。
    pub fn is_empty(&self) -> bool {
        false
    }

    /// 预过滤字节（全部为通配时为 `None`）。
    pub fn rare_byte(&self) -> Option<u8> {
        self.rare
    }

    #[inline]
    fn fold(&self, b: u8) -> u8 {
        if self.case_insensitive {
            b.to_ascii_lowercase()
        } else {
            b
        }
    }

    /// 验证起点为 `s` 的窗口；长度不足或确定不等时返回 `None`。
    #[inline]
    fn verify(&self, hay: &[u8], s: usize) -> Option<MatchVerdict> {
        if s + self.len > hay.len() {
            return None;
        }
        let mut has_unknown = false;
        for (j, p) in self.pattern.iter().enumerate() {
            match p {
                None => has_unknown = true,
                Some(c) if s + j < hay.len() && *c == self.fold(hay[s + j]) => {}
                Some(_) => return None,
            }
        }
        Some(if has_unknown {
            MatchVerdict::Unknown
        } else {
            MatchVerdict::Match
        })
    }

    /// 在 `hay` 中查找首个匹配；返回 `(起始偏移, 三值判定)`，偏移最小者优先。
    pub fn find(&self, hay: &[u8]) -> Option<(usize, MatchVerdict)> {
        if let Some(finder) = &self.finder {
            return finder.find(hay).map(|s| (s, MatchVerdict::Match));
        }
        let m = self.len;
        let mut start = 0;
        while start + m <= hay.len() {
            match self.rare {
                // 预过滤：窗口 [start, start+m) 必须在 start + rare_at 处命中稀有字节。
                Some(r) => {
                    let from = start + self.rare_at;
                    let tail = &hay[from..];
                    let q = if self.case_insensitive && r.is_ascii_alphabetic() {
                        memchr2(r, r.to_ascii_uppercase(), tail)
                    } else {
                        memchr(r, tail)
                    }?;
                    let s = from + q - self.rare_at;
                    if let Some(v) = self.verify(hay, s) {
                        return Some((s, v));
                    }
                    start = s + 1;
                }
                None => {
                    if let Some(v) = self.verify(hay, start) {
                        return Some((start, v));
                    }
                    start += 1;
                }
            }
        }
        None
    }

    /// 把一行映射为三值：命中确定模式为 `True`，仅通配命中为 `Unknown`，否则 `False`。
    pub fn line_verdict(&self, line: &[u8]) -> Ternary {
        match self.find(line) {
            Some((_, MatchVerdict::Match)) => Ternary::True,
            Some((_, MatchVerdict::Unknown)) => Ternary::Unknown,
            _ => Ternary::False,
        }
    }

    /// 批量判定：返回每行的三值结果。
    ///
    /// 行数 ≥ 2 且 `jobs > 1` 时用 [`parallel::map`] 分块并行；结果顺序与串行一致。
    pub fn judge_lines(&self, lines: &[&[u8]], jobs: usize) -> Vec<Ternary> {
        parallel::map(lines, jobs, |line| {
            Ok::<Ternary, ()>(self.line_verdict(line))
        })
        .expect("infallible")
    }

    /// 返回所有匹配（起始偏移 + 判定），按偏移升序；最多 `limit` 条，`limit == 0` 表示不限。
    pub fn find_all(&self, hay: &[u8], limit: usize) -> Vec<(usize, MatchVerdict)> {
        let mut out = Vec::new();
        let mut rest = hay;
        let mut base = 0;
        while let Some((off, verdict)) = self.find(rest) {
            out.push((base + off, verdict));
            if limit != 0 && out.len() >= limit {
                break;
            }
            rest = &rest[off + 1..];
            base += off + 1;
        }
        out
    }
}

/// 典型源码中的大致出现频率，越小越稀有；只用于预过滤字节选择。
#[inline]
fn rarity(b: u8) -> u8 {
    match b {
        b' ' | b'\t' => 9,
        b'e' | b't' | b'a' | b'o' | b'i' | b'n' | b's' | b'r' => 8,
        b'a'..=b'z' => 5,
        b'A'..=b'Z' => 4,
        b'0'..=b'9' => 3,
        b'_' | b'.' => 6,
        _ => 1,
    }
}

/// 朴素参考实现（穷举逐位置比较），作为预过滤实现的独立 oracle。
pub fn naive_find(
    pattern: &str,
    case_insensitive: bool,
    hay: &[u8],
) -> Option<(usize, MatchVerdict)> {
    let fold = |b: u8| {
        if case_insensitive {
            b.to_ascii_lowercase()
        } else {
            b
        }
    };
    let pat: Vec<Option<u8>> = pattern
        .as_bytes()
        .iter()
        .map(|&b| (b != b'?').then(|| fold(b)))
        .collect();
    let m = pat.len();
    if m == 0 || hay.len() < m {
        return None;
    }
    for i in 0..=(hay.len() - m) {
        let mut has_unknown = false;
        let mut ok = true;
        for (j, p) in pat.iter().enumerate() {
            match p {
                None => has_unknown = true,
                Some(c) if *c == fold(hay[i + j]) => {}
                Some(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return Some((
                i,
                if has_unknown {
                    MatchVerdict::Unknown
                } else {
                    MatchVerdict::Match
                },
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_match() {
        let s = TritSearcher::new("fn open", false).unwrap();
        assert_eq!(s.line_verdict(b"pub fn open(dir"), Ternary::True);
        assert_eq!(s.line_verdict(b"pub fn close(dir"), Ternary::False);
    }

    #[test]
    fn wildcard_is_unknown() {
        let s = TritSearcher::new("fn ?pen", false).unwrap();
        assert_eq!(s.line_verdict(b"fn open"), Ternary::Unknown);
        assert_eq!(s.line_verdict(b"fn xxxx"), Ternary::False);
    }

    #[test]
    fn case_insensitive() {
        let s = TritSearcher::new("ERROR", true).unwrap();
        assert_eq!(s.line_verdict(b"error: x"), Ternary::True);
        assert_eq!(s.line_verdict(b"ErRoR: x"), Ternary::True);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            TritSearcher::new("", false),
            Err(SearchError::Empty)
        ));
        assert!(matches!(
            TritSearcher::new(&"a".repeat(4097), false),
            Err(SearchError::TooLong(4097))
        ));
    }

    #[test]
    fn find_returns_earliest_offset() {
        let s = TritSearcher::new("ab", false).unwrap();
        assert_eq!(s.find(b"xxabyyab"), Some((2, MatchVerdict::Match)));
        assert_eq!(s.find(b"aba"), Some((0, MatchVerdict::Match)));
        let s = TritSearcher::new("?b", false).unwrap();
        assert_eq!(s.find(b"abcb"), Some((0, MatchVerdict::Unknown)));
    }

    #[test]
    fn parallel_matches_serial() {
        let s = TritSearcher::new("needle?", false).unwrap();
        let lines: Vec<&[u8]> = vec![
            b"hay",
            b"needles here",
            b"needleX there",
            b"nothing",
            b"a needle_ and needle9",
        ];
        let serial = s.judge_lines(&lines, 1);
        let parallel = s.judge_lines(&lines, 4);
        assert_eq!(serial, parallel);
        assert_eq!(serial[1], Ternary::Unknown);
        assert_eq!(serial[4], Ternary::Unknown);
    }

    #[test]
    fn all_wildcards() {
        let s = TritSearcher::new("???", false).unwrap();
        assert_eq!(s.rare_byte(), None);
        assert_eq!(s.find(b"ab"), None);
        assert_eq!(s.find(b"abc"), Some((0, MatchVerdict::Unknown)));
    }

    #[test]
    fn agrees_with_naive_oracle() {
        let patterns = [
            "fn open", "a?c", "???", "x?y?z", "?ab", "ab?", "kk", "_i?d", "??a?", "zzzz", "?",
            "k?", "f?", "?f", "f?f",
        ];
        let hays: Vec<Vec<u8>> = [
            "",
            "a",
            "abc",
            "xxabcxxabc",
            "aXc a?c",
            "fn open fn opEn fn  open",
            "aXc",
            "a_c",
            "the quick brown fox",
            "zzzzzzzz",
            "needle in haystack",
            "k",
            "kk",
            "fA",
            "Af",
            "fAf",
            "fff",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        for p in patterns {
            for ci in [false, true] {
                let s = TritSearcher::new(p, ci).unwrap();
                for h in &hays {
                    assert_eq!(
                        s.find(h),
                        naive_find(p, ci, h),
                        "pattern={p:?} ci={ci} hay={h:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn proptest_against_naive() {
        use proptest::prelude::*;
        proptest!(|(pat in "[a-zA-Z?]{1,8}", hay in "[a-zA-Z ]{0,40}", ci in any::<bool>())| {
            let s = TritSearcher::new(&pat, ci).unwrap();
            prop_assert_eq!(s.find(hay.as_bytes()), naive_find(&pat, ci, hay.as_bytes()));
        });
    }

    /// 不变量：模式不含 `?` 时永远不会返回 `Unknown`。
    #[test]
    fn invariant_no_unknown_without_wildcard() {
        use proptest::prelude::*;
        proptest!(|(pat in "[a-z]{1,8}", hay in "[a-z ]{0,40}")| {
            let s = TritSearcher::new(&pat, false).unwrap();
            prop_assert!(s.find(hay.as_bytes()).map(|(_, v)| v) != Some(MatchVerdict::Unknown));
        });
    }

    #[test]
    fn find_all_lists_every_offset_in_order() {
        let s = TritSearcher::new("ab", false).unwrap();
        assert_eq!(
            s.find_all(b"ababab", 0),
            vec![
                (0, MatchVerdict::Match),
                (2, MatchVerdict::Match),
                (4, MatchVerdict::Match)
            ]
        );
        assert_eq!(s.find_all(b"ababab", 2).len(), 2);
        assert!(s.find_all(b"zzz", 0).is_empty());

        let wildcard = TritSearcher::new("a?", false).unwrap();
        assert_eq!(
            wildcard.find_all(b"aXaY", 0),
            vec![(0, MatchVerdict::Unknown), (2, MatchVerdict::Unknown)]
        );
    }

    /// `find_all` 必须与「朴素 oracle 逐次跳过起点」的参考扫描完全一致。
    #[test]
    fn find_all_agrees_with_naive_scan() {
        use proptest::prelude::*;
        proptest!(|(pat in "[a-z?]{1,6}", hay in "[a-z]{0,30}")| {
            let s = TritSearcher::new(&pat, false).unwrap();
            let bytes = hay.as_bytes();
            let mut expected = Vec::new();
            let mut base = 0usize;
            while base <= bytes.len() {
                match naive_find(&pat, false, &bytes[base..]) {
                    Some((off, verdict)) => {
                        expected.push((base + off, verdict));
                        base += off + 1;
                    }
                    None => break,
                }
            }
            prop_assert_eq!(s.find_all(bytes, 0), expected);
        });
    }

    /// 并发批次判定必须与串行结果逐行一致，且大量输入下保持顺序。
    #[test]
    fn parallel_judgement_agrees_with_serial_on_large_input() {
        let s = TritSearcher::new("error: ?", false).unwrap();
        let corpus: Vec<String> = (0..2_000)
            .map(|i| {
                if i % 3 == 0 {
                    format!("error: code {i}")
                } else {
                    format!("row {i} all good")
                }
            })
            .collect();
        let lines: Vec<&[u8]> = corpus.iter().map(|line| line.as_bytes()).collect();
        let serial = s.judge_lines(&lines, 1);
        for jobs in [2, 4, 8, 0, 64] {
            assert_eq!(serial, s.judge_lines(&lines, jobs), "jobs={jobs}");
        }
        assert_eq!(
            serial.iter().filter(|v| **v == Ternary::Unknown).count(),
            667
        );
        assert_eq!(
            serial.iter().filter(|v| **v == Ternary::False).count(),
            1_333
        );
    }
}
