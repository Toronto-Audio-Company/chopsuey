//! Licence keys for Toronto Audio Company plugins: the payload layout, the
//! pasteable text form and the Ed25519 signature check.
//!
//! Shared by the plugin (which only ever verifies, with an embedded public
//! key) and `tools/licence_keygen` (which issues keys with the private key),
//! so the two can never disagree about the format.
//!
//! # Text form
//!
//! `CHOPSUEY-XXXXX-XXXXX-...`: the product id in capitals, then the signed
//! bytes in Crockford base32, in dash-separated groups of five. When reading
//! a key, case, whitespace (including line breaks), dashes and a missing
//! prefix are all tolerated, and the look-alike letters O, I and L are read as
//! 0, 1 and 1.
//!
//! # Signed bytes (format version 1)
//!
//! ```text
//! u8        format version (1)
//! u8 + n    product id, ASCII, length-prefixed (e.g. "chopsuey")
//! u32 BE    key id (lets a leaked key be revoked)
//! u32 BE    issue date, days since 1970-01-01 UTC
//! u8 + n    licensee name, UTF-8, length-prefixed
//! u8 + n    licensee email, UTF-8, length-prefixed
//! [64]      Ed25519 signature over SIGNING_CONTEXT ++ all of the above
//! ```

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use std::fmt;

/// The payload layout this crate writes and the only one it reads.
pub const FORMAT_VERSION: u8 = 1;
/// Prepended to the payload before signing, so a signature made for a licence
/// can never be mistaken for one over some other kind of message.
pub const SIGNING_CONTEXT: &[u8] = b"Toronto Audio Company licence\0";
/// Characters per dash-separated group in the text form.
pub const GROUP_LEN: usize = 5;

const SIGNATURE_LEN: usize = 64;
const MAX_PRODUCT_LEN: usize = 32;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// What a licence key says, once its signature has checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Licence {
    pub version: u8,
    /// Lower-case product id, e.g. `"chopsuey"`.
    pub product: String,
    pub key_id: u32,
    /// Issue date in days since 1970-01-01 UTC.
    pub issued_days: u32,
    pub name: String,
    pub email: String,
}

impl Licence {
    /// A licence in the current format version.
    pub fn new(product: &str, name: &str, email: &str, key_id: u32, issued_days: u32) -> Self {
        Self {
            version: FORMAT_VERSION,
            product: product.to_string(),
            key_id,
            issued_days,
            name: name.to_string(),
            email: email.to_string(),
        }
    }
}

/// Why a pasted key was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Not base32, empty, or far too short to hold a signature.
    NotAKey,
    /// The signature doesn't match: mistyped, truncated, edited or forged.
    BadSignature,
    /// Genuinely signed, but in a payload layout this build doesn't know.
    UnsupportedVersion(u8),
    /// Genuinely signed, but the payload doesn't parse. Only possible if the
    /// issuer itself is broken.
    Malformed,
    /// Genuinely signed, but for another product (named here).
    WrongProduct(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::NotAKey => write!(f, "not a licence key"),
            KeyError::BadSignature => write!(f, "licence key signature does not match"),
            KeyError::UnsupportedVersion(v) => write!(f, "unsupported licence format version {v}"),
            KeyError::Malformed => write!(f, "licence key payload is malformed"),
            KeyError::WrongProduct(p) => write!(f, "licence key is for {p}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Why a licence couldn't be issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    BadProduct,
    FieldTooLong(&'static str),
    EmptyField(&'static str),
}

impl fmt::Display for IssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueError::BadProduct => write!(
                f,
                "product id must be 1-{MAX_PRODUCT_LEN} lower-case ASCII letters, digits or underscores"
            ),
            IssueError::FieldTooLong(field) => write!(f, "{field} is longer than 255 bytes"),
            IssueError::EmptyField(field) => write!(f, "{field} is empty"),
        }
    }
}

impl std::error::Error for IssueError {}

/// The public key belonging to a 32-byte Ed25519 private key (seed).
pub fn public_key_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Signs `licence` with the private key `seed` and returns the pasteable key.
pub fn issue(licence: &Licence, seed: &[u8; 32]) -> Result<String, IssueError> {
    let mut bytes = encode_payload(licence)?;
    let signature = SigningKey::from_bytes(seed).sign(&signed_message(&bytes));
    bytes.extend_from_slice(&signature.to_bytes());
    Ok(format_key(&licence.product, &bytes))
}

/// Checks a pasted key against `public_key` and returns what it says.
///
/// The signature is checked before anything in the payload is trusted, so the
/// errors after [`KeyError::BadSignature`] only ever describe genuine keys.
pub fn verify(key: &str, expected_product: &str, public_key: &[u8; 32]) -> Result<Licence, KeyError> {
    let bytes = parse_key(key, expected_product).ok_or(KeyError::NotAKey)?;
    if bytes.len() <= SIGNATURE_LEN {
        return Err(KeyError::NotAKey);
    }
    let (payload, sig) = bytes.split_at(bytes.len() - SIGNATURE_LEN);
    let sig: [u8; SIGNATURE_LEN] = sig.try_into().map_err(|_| KeyError::NotAKey)?;
    let verifying = VerifyingKey::from_bytes(public_key).map_err(|_| KeyError::BadSignature)?;
    verifying
        .verify_strict(&signed_message(payload), &Signature::from_bytes(&sig))
        .map_err(|_| KeyError::BadSignature)?;

    let licence = decode_payload(payload)?;
    if licence.product != expected_product {
        return Err(KeyError::WrongProduct(licence.product));
    }
    Ok(licence)
}

fn signed_message(payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SIGNING_CONTEXT.len() + payload.len());
    msg.extend_from_slice(SIGNING_CONTEXT);
    msg.extend_from_slice(payload);
    msg
}

/// The unsigned payload bytes, in the layout described at the top of the file.
pub fn encode_payload(licence: &Licence) -> Result<Vec<u8>, IssueError> {
    let product_ok = !licence.product.is_empty()
        && licence.product.len() <= MAX_PRODUCT_LEN
        && licence
            .product
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !product_ok {
        return Err(IssueError::BadProduct);
    }
    let name = licence.name.trim();
    let email = licence.email.trim();
    if name.is_empty() {
        return Err(IssueError::EmptyField("name"));
    }
    if email.is_empty() {
        return Err(IssueError::EmptyField("email"));
    }

    let mut out = vec![licence.version];
    push_str(&mut out, &licence.product, "product")?;
    out.extend_from_slice(&licence.key_id.to_be_bytes());
    out.extend_from_slice(&licence.issued_days.to_be_bytes());
    push_str(&mut out, name, "name")?;
    push_str(&mut out, email, "email")?;
    Ok(out)
}

fn push_str(out: &mut Vec<u8>, s: &str, field: &'static str) -> Result<(), IssueError> {
    let len = u8::try_from(s.len()).map_err(|_| IssueError::FieldTooLong(field))?;
    out.push(len);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Parses payload bytes (without the signature).
pub fn decode_payload(bytes: &[u8]) -> Result<Licence, KeyError> {
    let mut r = Reader(bytes);
    let version = r.u8().ok_or(KeyError::Malformed)?;
    if version != FORMAT_VERSION {
        return Err(KeyError::UnsupportedVersion(version));
    }
    let product = r.string().ok_or(KeyError::Malformed)?;
    let key_id = r.u32().ok_or(KeyError::Malformed)?;
    let issued_days = r.u32().ok_or(KeyError::Malformed)?;
    let name = r.string().ok_or(KeyError::Malformed)?;
    let email = r.string().ok_or(KeyError::Malformed)?;
    if !r.0.is_empty() {
        return Err(KeyError::Malformed);
    }
    Ok(Licence { version, product, key_id, issued_days, name, email })
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)?.try_into().ok().map(u32::from_be_bytes)
    }
    fn string(&mut self) -> Option<String> {
        let len = self.u8()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

/// `PRODUCT-XXXXX-XXXXX-...`
fn format_key(product: &str, bytes: &[u8]) -> String {
    let body = base32_encode(bytes);
    let mut out = product.to_ascii_uppercase();
    for group in body.as_bytes().chunks(GROUP_LEN) {
        out.push('-');
        // Base32 output is ASCII, so this can't split a character.
        out.push_str(std::str::from_utf8(group).unwrap_or_default());
    }
    out
}

fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '\u{2010}'..='\u{2015}' | '\u{2212}')
}

/// Undoes [`format_key`], forgivingly. `None` if it isn't base32 at all.
fn parse_key(key: &str, expected_product: &str) -> Option<Vec<u8>> {
    let upper = key.trim().to_uppercase();
    let joined: String = upper.chars().filter(|c| !is_separator(*c)).collect();

    // A leading word longer than a group, before the first separator, is a
    // product prefix (possibly another product's: the signed payload decides).
    let head = upper.split(is_separator).next().unwrap_or("");
    let expected = expected_product.to_ascii_uppercase();
    let body = if head.len() > GROUP_LEN && head.len() <= MAX_PRODUCT_LEN && head.chars().all(|c| c.is_ascii_alphabetic()) {
        &joined[head.len()..]
    } else if let Some(rest) = joined.strip_prefix(expected.as_str()) {
        rest
    } else {
        &joined
    };
    base32_decode(body)
}

/// Crockford base32, no padding, most significant bit first.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(CROCKFORD[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Inverse of [`base32_encode`]. Accepts lower case and reads O as 0 and I/L
/// as 1. Rejects other characters and non-zero padding bits, so every byte
/// string has exactly one canonical spelling.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars() {
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let v = CROCKFORD.iter().position(|&a| a as char == c)? as u32;
        acc = ((acc << 5) | v) & 0xFFFF;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits are padding: there must be fewer than 5 and all zero.
    if bits >= 5 || acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];

    fn sample() -> Licence {
        Licence::new("chopsuey", "Ada Lovelace", "ada@example.com", 42, 20_000)
    }

    #[test]
    fn base32_round_trips_every_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..40 {
            let enc = base32_encode(&data[..n]);
            assert_eq!(base32_decode(&enc).unwrap(), &data[..n], "len {n}");
            assert_eq!(base32_decode(&enc.to_lowercase()).unwrap(), &data[..n]);
        }
    }

    #[test]
    fn base32_reads_lookalikes_and_rejects_junk() {
        assert_eq!(base32_decode("OO"), base32_decode("00"));
        assert_eq!(base32_decode("1I"), base32_decode("1L"));
        assert_eq!(base32_decode("U0"), None);
        assert_eq!(base32_decode("0!"), None);
        // "01" leaves a set padding bit: not a canonical encoding.
        assert_eq!(base32_decode("01"), None);
    }

    #[test]
    fn issue_then_verify() {
        let key = issue(&sample(), &SEED).unwrap();
        assert!(key.starts_with("CHOPSUEY-"));
        assert!(key["CHOPSUEY-".len()..].split('-').all(|g| g.len() <= GROUP_LEN));
        let public = public_key_from_seed(&SEED);
        assert_eq!(verify(&key, "chopsuey", &public).unwrap(), sample());
    }

    #[test]
    fn verify_tolerates_how_people_paste() {
        let key = issue(&sample(), &SEED).unwrap();
        let public = public_key_from_seed(&SEED);
        let body = key.strip_prefix("CHOPSUEY-").unwrap();
        let variants = [
            key.to_lowercase(),
            format!("  {key}\n"),
            key.replace('-', ""),
            key.replace('-', " "),
            body.to_string(),
            body.replace('-', ""),
            key.replace('-', "\u{2013}"),
            // Wrapped mid-group by an email client.
            format!("{}\r\n{}", &key[..30], &key[30..]),
            format!("CHOPS\nUEY{}", &key[8..]),
        ];
        for v in variants {
            assert_eq!(verify(&v, "chopsuey", &public).unwrap(), sample(), "{v:?}");
        }
    }

    #[test]
    fn rejects_tampering() {
        let key = issue(&sample(), &SEED).unwrap();
        let public = public_key_from_seed(&SEED);
        // Flip each data character in turn; every single change must fail.
        let prefix = "CHOPSUEY-".len();
        for i in prefix..key.len() {
            let c = key.as_bytes()[i];
            if c == b'-' {
                continue;
            }
            let replacement = if c == b'2' { '3' } else { '2' };
            let mut tampered = key.clone();
            tampered.replace_range(i..i + 1, &replacement.to_string());
            assert!(verify(&tampered, "chopsuey", &public).is_err(), "position {i}");
        }
        // Truncated and extended keys.
        assert!(verify(&key[..key.len() - 5], "chopsuey", &public).is_err());
        assert!(verify(&format!("{key}-00000"), "chopsuey", &public).is_err());
    }

    #[test]
    fn rejects_other_signers() {
        let key = issue(&sample(), &[8; 32]).unwrap();
        let public = public_key_from_seed(&SEED);
        assert_eq!(verify(&key, "chopsuey", &public), Err(KeyError::BadSignature));
    }

    #[test]
    fn rejects_garbage() {
        let public = public_key_from_seed(&SEED);
        for junk in ["", "   ", "CHOPSUEY-", "hello world", "CHOPSUEY-ABCDE", "UUUUU-UUUUU"] {
            assert_eq!(verify(junk, "chopsuey", &public), Err(KeyError::NotAKey), "{junk:?}");
        }
    }

    #[test]
    fn reports_wrong_product() {
        let mut other = sample();
        other.product = "otherplug".into();
        let key = issue(&other, &SEED).unwrap();
        assert!(key.starts_with("OTHERPLUG-"));
        let public = public_key_from_seed(&SEED);
        assert_eq!(
            verify(&key, "chopsuey", &public),
            Err(KeyError::WrongProduct("otherplug".into()))
        );
    }

    #[test]
    fn reports_unknown_versions() {
        let mut future = sample();
        future.version = 2;
        let key = issue(&future, &SEED).unwrap();
        let public = public_key_from_seed(&SEED);
        assert_eq!(verify(&key, "chopsuey", &public), Err(KeyError::UnsupportedVersion(2)));
    }

    #[test]
    fn issue_validates_fields() {
        let mut l = sample();
        l.name = "x".repeat(256);
        assert_eq!(issue(&l, &SEED), Err(IssueError::FieldTooLong("name")));
        let mut l = sample();
        l.email = "  ".into();
        assert_eq!(issue(&l, &SEED), Err(IssueError::EmptyField("email")));
        let mut l = sample();
        l.product = "Chop Suey".into();
        assert_eq!(issue(&l, &SEED), Err(IssueError::BadProduct));
    }

    #[test]
    fn unicode_names_survive() {
        let l = Licence::new("chopsuey", "Zoë Ångström 李", "zoe@example.com", 1, 1);
        let key = issue(&l, &SEED).unwrap();
        assert_eq!(verify(&key, "chopsuey", &public_key_from_seed(&SEED)).unwrap(), l);
    }
}
