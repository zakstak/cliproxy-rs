//! AWS Signature Version 4 for S3.

use crate::text::GoStr;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

pub(crate) const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(data).finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// minio-go `s3utils.EncodePath`: everything but unreserved characters and `/`.
pub(crate) fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Go `url.QueryEscape` with `+` spelled `%20`, as minio-go canonicalizes queries.
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Go `url.Values.Encode` order: keys sorted, values in insertion order.
pub(crate) fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<&(String, String)> = query.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// minio-go `signV4TrimAll`: inner whitespace runs collapse to one space.
fn trim_all(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) struct Request<'a> {
    pub method: &'a str,
    /// The decoded object path, `/bucket/key`.
    pub path: &'a str,
    pub query: &'a [(String, String)],
    /// Headers to sign besides `host` (names in any case), without `Authorization`.
    pub headers: &'a [(String, String)],
    pub host: &'a str,
    pub payload_sha256: &'a str,
}

/// Returns `(x-amz-date, Authorization)`.
pub(crate) fn sign(
    request: &Request,
    access_key: &str,
    secret_key: &str,
    region: &str,
    now: DateTime<Utc>,
) -> (String, String) {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = now.format("%Y%m%d").to_string();
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.go_lower(), trim_all(v)))
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "authorization" | "user-agent" | "accept-encoding" | "host" | "x-amz-date"
            )
        })
        .collect();
    headers.push(("host".into(), request.host.to_owned()));
    headers.push(("x-amz-date".into(), amz_date.clone()));
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = [
        request.method,
        &encode_path(request.path),
        &canonical_query(request.query),
        &canonical_headers,
        &signed_headers,
        request.payload_sha256,
    ]
    .join("\n");
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let key = [region.as_bytes(), b"s3", b"aws4_request"].iter().fold(
        hmac(format!("AWS4{secret_key}").as_bytes(), day.as_bytes()),
        |key, part| hmac(&key, part),
    );
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    (amz_date, authorization)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn s3_signatures_match_independent_aws_canonical_request_vectors() {
        // AWS SigV4 is the authority. The companion Python standard-library script
        // signs explicit canonical requests without executing this implementation.
        let doc: Value =
            serde_json::from_str(include_str!("../../tests/storage/fixtures/sigv4_contract.json")).unwrap();
        let date = doc["date"].as_str().unwrap();
        let now = chrono::NaiveDateTime::parse_from_str(date, "%Y%m%dT%H%M%SZ")
            .unwrap()
            .and_utc();
        for case in doc["cases"].as_array().unwrap() {
            let query: Vec<(String, String)> = serde_json::from_value(case["query"].clone()).unwrap();
            let headers: Vec<(String, String)> = serde_json::from_value(case["headers"].clone()).unwrap();
            let request = Request {
                method: case["method"].as_str().unwrap(),
                path: case["path"].as_str().unwrap(),
                query: &query,
                headers: &headers,
                host: case["host"].as_str().unwrap(),
                payload_sha256: case["payload_sha256"].as_str().unwrap(),
            };
            let canonical = case["canonical_request"]
                .as_str()
                .unwrap()
                .split('\n')
                .collect::<Vec<_>>();
            assert_eq!(encode_path(request.path), canonical[1]);
            assert_eq!(canonical_query(&query), canonical[2]);
            let (actual_date, actual) = sign(
                &request,
                doc["access_key"].as_str().unwrap(),
                doc["secret_key"].as_str().unwrap(),
                case["region"].as_str().unwrap(),
                now,
            );
            assert_eq!(actual_date, date);
            assert_eq!(actual, case["authorization"], "{} {}", request.method, request.path);
            // Header order, casing and insignificant whitespace do not change the signature.
            let mut reordered: Vec<_> = headers
                .iter()
                .rev()
                .map(|(key, value)| (key.to_ascii_uppercase(), format!("  {value}  ")))
                .collect();
            reordered.push(("User-Agent".into(), "different client".into()));
            reordered.push(("Authorization".into(), "must be ignored".into()));
            let altered = Request {
                headers: &reordered,
                ..request
            };
            assert_eq!(
                sign(
                    &altered,
                    doc["access_key"].as_str().unwrap(),
                    doc["secret_key"].as_str().unwrap(),
                    case["region"].as_str().unwrap(),
                    now
                )
                .1,
                actual
            );
        }
    }
}
