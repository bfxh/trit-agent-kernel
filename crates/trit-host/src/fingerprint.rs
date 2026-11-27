//! 内容指纹：FNV-1a 64，用于同一可见窗口内的重复检测。
//!
//! 只服务污染门（同轮重复上下文/重复工具输出），不承担跨版本稳定性承诺。

/// 返回 `fnv1a64:<16 位十六进制>`。空输入也会得到一个确定值，由调用方决定是否使用。
pub(crate) fn fnv1a64(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_distinguishes_content() {
        assert_eq!(fnv1a64(b""), "fnv1a64:cbf29ce484222325");
        assert_eq!(fnv1a64(b"abc"), fnv1a64(b"abc"));
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert!(fnv1a64(b"abc").starts_with("fnv1a64:"));
    }
}
