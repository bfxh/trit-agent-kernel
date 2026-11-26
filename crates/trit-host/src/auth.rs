//! Bearer 认证的共享比较逻辑。
//!
//! HTTP 头的字符串相等比较会在第一个不同字节处提前返回，攻击者可利用响应时间
//! 猜测 token 前缀。这里先把期望头与实际头都压成固定长度的 SHA-256 摘要，再用
//! `subtle` 做常量时间比较；认证路径只比较摘要，不把完整 token 带进比较逻辑。

use ring::digest::{Context, SHA256};
use subtle::ConstantTimeEq;

pub(crate) const AUTH_DIGEST_LEN: usize = 32;

/// 计算 `Bearer <token>` 的 SHA-256 摘要，供服务端启动时缓存。
pub(crate) fn bearer_digest(token: &str) -> [u8; AUTH_DIGEST_LEN] {
    let mut context = Context::new(&SHA256);
    context.update(b"Bearer ");
    context.update(token.as_bytes());
    let digest = context.finish();
    let mut bytes = [0u8; AUTH_DIGEST_LEN];
    bytes.copy_from_slice(digest.as_ref());
    bytes
}

/// 把实际 `Authorization` 头压成同长度摘要后做常量时间比较。
pub(crate) fn authorization_matches(
    header: Option<&str>,
    expected: &[u8; AUTH_DIGEST_LEN],
) -> bool {
    let Some(header) = header else {
        return false;
    };
    let actual = {
        let mut context = Context::new(&SHA256);
        context.update(header.as_bytes());
        context.finish()
    };
    bool::from(actual.as_ref().ct_eq(&expected[..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_digest_matches_only_the_exact_header() {
        let expected = bearer_digest("s3cret");
        assert!(authorization_matches(Some("Bearer s3cret"), &expected));
        assert!(!authorization_matches(Some("Bearer s3cre"), &expected));
        assert!(!authorization_matches(Some("Bearer s3cretx"), &expected));
        assert!(!authorization_matches(Some("Bearer "), &expected));
        assert!(!authorization_matches(None, &expected));
    }
}
