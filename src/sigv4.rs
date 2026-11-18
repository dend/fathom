//! AWS Signature Version 4, as used by R2's S3-compatible API.
//!
//! Only what the app needs: header-based signing for API calls and
//! query-string signing for presigned GET URLs.

use sha2::{Digest, Sha256};

/// Payload hash placeholder that lets request bodies be streamed without hashing them first.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";

#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);

    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
/// `/` is kept as-is unless `encode_slash` is set.
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Decodes percent-encoding. `+` is treated as a space when `plus_as_space` is set.
pub fn uri_decode(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' if plus_as_space => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Builds the canonical (sorted, encoded) query string.
pub fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Formats a Unix timestamp as `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;

    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn signing_key(creds: &Credentials, date: &str) -> [u8; 32] {
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, creds.region.as_bytes());
    let k_service = hmac(&k_region, SERVICE.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn sign(
    creds: &Credentials,
    amz_date: &str,
    method: &str,
    canonical_path: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String, String) {
    let date = &amz_date[..8];
    let scope = format!("{date}/{}/{SERVICE}/aws4_request", creds.region);

    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    sorted.sort();

    let canonical_headers: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = hex(&hmac(&signing_key(creds, date), string_to_sign.as_bytes()));
    (signature, signed_headers, scope)
}

/// Returns the value of the `Authorization` header for a request.
///
/// `headers` must contain every header to sign (at minimum `host`,
/// `x-amz-date` and `x-amz-content-sha256`). `canonical_path` and
/// `canonical_query` must already be encoded.
pub fn authorization(
    creds: &Credentials,
    amz_date: &str,
    method: &str,
    canonical_path: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let (signature, signed_headers, scope) = sign(
        creds,
        amz_date,
        method,
        canonical_path,
        canonical_query,
        headers,
        payload_hash,
    );
    format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

/// Returns the full query string (including the signature) of a presigned URL.
pub fn presign_query(
    creds: &Credentials,
    amz_date: &str,
    method: &str,
    host: &str,
    canonical_path: &str,
    extra_query: &[(&str, &str)],
    expires_secs: u64,
) -> String {
    let date = &amz_date[..8];
    let credential = format!(
        "{}/{date}/{}/{SERVICE}/aws4_request",
        creds.access_key_id, creds.region
    );
    let expires = expires_secs.to_string();

    let mut params: Vec<(&str, &str)> = vec![
        ("X-Amz-Algorithm", ALGORITHM),
        ("X-Amz-Credential", &credential),
        ("X-Amz-Date", amz_date),
        ("X-Amz-Expires", &expires),
        ("X-Amz-SignedHeaders", "host"),
    ];
    params.extend_from_slice(extra_query);
    let query = canonical_query(&params);

    let headers = [("host".to_string(), host.to_string())];
    let (signature, _, _) = sign(
        creds,
        amz_date,
        method,
        canonical_path,
        &query,
        &headers,
        UNSIGNED_PAYLOAD,
    );
    format!("{query}&X-Amz-Signature={signature}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    // Example values from the AWS SigV4 documentation.
    fn example_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
        }
    }

    fn h(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn sha256_of_empty_input() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signs_get_object() {
        let auth = authorization(
            &example_creds(),
            "20130524T000000Z",
            "GET",
            "/test.txt",
            "",
            &[
                h("host", "examplebucket.s3.amazonaws.com"),
                h("range", "bytes=0-9"),
                h("x-amz-content-sha256", EMPTY_SHA256),
                h("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn signs_list_objects_with_query() {
        let auth = authorization(
            &example_creds(),
            "20130524T000000Z",
            "GET",
            "/",
            &canonical_query(&[("prefix", "J"), ("max-keys", "2")]),
            &[
                h("host", "examplebucket.s3.amazonaws.com"),
                h("x-amz-content-sha256", EMPTY_SHA256),
                h("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
        );
        assert!(auth.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn presigns_get_object() {
        let query = presign_query(
            &example_creds(),
            "20130524T000000Z",
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            86_400,
        );
        assert_eq!(
            query,
            "X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn uri_encoding_round_trips() {
        let key = "photos/2024/été & more/file (1)+final.png";
        let encoded = uri_encode(key, false);
        assert_eq!(
            encoded,
            "photos/2024/%C3%A9t%C3%A9%20%26%20more/file%20%281%29%2Bfinal.png"
        );
        assert_eq!(uri_decode(&encoded, true), key);
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_decode("a+b%2Bc", true), "a b+c");
        assert_eq!(uri_decode("100%", true), "100%");
        assert_eq!(uri_decode("%zz%4", false), "%zz%4");
    }

    #[test]
    fn formats_amz_dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(amz_date(1_709_251_199), "20240229T235959Z");
        assert_eq!(amz_date(4_102_444_800), "21000101T000000Z");
    }
}
