//! TOTP (RFC 6238), the second factor of password logins, and recovery codes.
//! See docs/decisions.md, Authentication.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::Digest;
use totp_rs::{Builder, Secret, Totp};
use uuid::Uuid;

/// How many recovery codes an account gets.
pub const RECOVERY_CODES: usize = 10;

/// A new random secret (160 bits, the RFC's recommendation for SHA-1).
pub fn generate_secret() -> [u8; 20] {
    let mut secret = [0u8; 20];
    getrandom::fill(&mut secret).expect("system randomness");
    secret
}

/// 6 digits, 30-second steps, one step of skew either way, SHA-1: what every
/// authenticator app supports.
fn totp(secret: &[u8], issuer: &str, email: &str) -> Totp {
    // ':' separates issuer and account in otpauth URIs.
    Builder::new()
        .with_secret(Secret::new(secret.into()))
        .with_issuer(Some(issuer.replace(':', "")))
        .with_account_name(email.replace(':', ""))
        .build()
        .expect("valid parameters")
}

/// The secret in base32 (typed by hand) and the `otpauth://` URI (QR code).
pub fn setup(secret: &[u8], issuer: &str, email: &str) -> (String, String) {
    let totp = totp(secret, issuer, email);
    let base32 = Secret::new(secret.into()).to_base32();
    (base32, totp.to_url().expect("valid parameters"))
}

/// The time step a code is valid for (now, or one step before or after), if any.
pub fn check(secret: &[u8], code: &str) -> Option<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    totp(secret, "x", "x")
        .check(code.trim(), now)
        .and_then(|step| i64::try_from(step).ok())
}

/// Whether a code looks like a recovery code rather than a TOTP code.
pub fn is_recovery_code(code: &str) -> bool {
    !normalize(code).chars().all(|c| c.is_ascii_digit())
}

/// A new recovery code: 16 base32 characters (80 bits) in groups of 4.
pub fn generate_recovery_code() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(b % 32)] as char)
        .collect();
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// What's stored for a recovery code: `(prefix, SHA-256)`, salted with the
/// account. The prefix (from the hash) is the `token` lookup key.
pub fn recovery_code_hash(account: Uuid, code: &str) -> (String, [u8; 32]) {
    let hash: [u8; 32] = sha2::Sha256::new()
        .chain_update(account.as_bytes())
        .chain_update(normalize(code).as_bytes())
        .finalize()
        .into();
    let prefix = hash[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    (format!("rc_{prefix}"), hash)
}

/// Lowercase, without separators: codes can be typed loosely.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let secret = generate_secret();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let code = totp(&secret, "x", "x").generate(now).to_string();
        assert_eq!(check(&secret, &code), Some((now / 30) as i64));
        let old = totp(&secret, "x", "x").generate(now - 300).to_string();
        if old != code {
            assert_eq!(check(&secret, &old), None, "outside the window");
        }
        assert_eq!(check(&secret, "abc"), None);
        assert!(!is_recovery_code(&code));

        let (base32, uri) = setup(&secret, "My: cloud", "a@x.ch");
        assert_eq!(base32.len(), 32);
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains(&base32));
    }

    #[test]
    fn recovery_codes() {
        let code = generate_recovery_code();
        assert_eq!(code.len(), 19);
        assert!(is_recovery_code(&code));
        let account = Uuid::from_u128(1);
        let loose = code.to_uppercase().replace('-', " ");
        assert_eq!(
            recovery_code_hash(account, &code),
            recovery_code_hash(account, &loose)
        );
        assert_ne!(
            recovery_code_hash(account, &code),
            recovery_code_hash(Uuid::from_u128(2), &code),
            "salted with the account"
        );
    }
}
