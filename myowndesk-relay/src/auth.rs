use constant_time_eq::constant_time_eq;
use ring::hmac;

/// 计算 HMAC-SHA256(key, device_id)，返回 32 字节认证令牌
pub fn compute_token(key: &[u8], device_id: &str) -> Vec<u8> {
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&hmac_key, device_id.as_bytes());
    tag.as_ref().to_vec()
}

/// 验证 auth_token，使用 constant-time 比较防止计时攻击
pub fn verify_token(key: &[u8], device_id: &str, token: &[u8]) -> bool {
    let expected = compute_token(key, device_id);
    // 长度不同直接返回 false（不会泄露信息，因为 compute_token 总是 32 字节）
    if expected.len() != token.len() {
        return false;
    }
    constant_time_eq(&expected, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_and_verify() {
        let key = b"my_secret_key_32_bytes_long!!";
        let device_id = "van-pc";
        let token = compute_token(key, device_id);
        assert!(verify_token(key, device_id, &token));
    }

    #[test]
    fn test_verify_wrong_key() {
        let key = b"my_secret_key_32_bytes_long!!";
        let wrong_key = b"wrong_secret_key_32_bytes_long";
        let device_id = "van-pc";
        let token = compute_token(key, device_id);
        assert!(!verify_token(wrong_key, device_id, &token));
    }

    #[test]
    fn test_verify_wrong_device() {
        let key = b"my_secret_key_32_bytes_long!!";
        let token = compute_token(key, "van-pc");
        assert!(!verify_token(key, "van-laptop", &token));
    }

    #[test]
    fn test_verify_wrong_length_token() {
        let key = b"my_secret_key_32_bytes_long!!";
        assert!(!verify_token(key, "van-pc", b"short"));
    }
}
