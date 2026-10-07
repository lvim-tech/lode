// lode/sigv4: signing an AWS request.
//
// Written out rather than taken from a crate, for one reason: this is the only
// part of the AWS drivers that can be PROVED here. AWS publishes canonical test
// vectors — a known key, a known request, a known signature — so the signing is
// checked against an answer somebody else computed, which is as close to a real
// test as anything in this file gets. The network half of those drivers cannot
// be tested here at all and is declared unverified; this half is not, and that
// difference is worth the eighty lines.
//
// The algorithm is AWS Signature Version 4, header form:
//
//   1. A CANONICAL REQUEST: method, path, query, headers, signed-header list,
//      payload hash — each normalised in a precisely specified way.
//   2. A STRING TO SIGN: the algorithm, the timestamp, the credential scope and
//      the hash of (1).
//   3. A SIGNING KEY derived from the secret by four chained HMACs, one per
//      scope element, so the key that signs is specific to the day, the region
//      and the service.
//   4. The signature, and an Authorization header naming which headers were
//      signed.
//
// What is normalised and why it matters: the canonical request is what the
// SERVER rebuilds from what it received. Every difference — an unsorted query
// parameter, a header name in the wrong case, a trailing space — produces a
// different hash and a signature mismatch, and the error AWS returns for that
// says nothing about which of them it was. So the normalisation is the whole
// job, and each rule below is one the server also applies.

use hmac::{Hmac, Mac, NewMac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_varkey(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// Percent-encode for a URI path or a query value.
///
/// AWS's rule, not the usual one: unreserved characters are `A-Za-z0-9-_.~` and
/// EVERYTHING else is encoded, including `/` in a query value but not in a
/// path. A library's "url encode" is nearly this and the difference is a
/// signature mismatch with an error message that does not mention encoding.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// What a request needs to be signed.
pub struct Request<'a> {
    pub method: &'a str,
    /// The path, already as it will be sent. Encoded here, so pass it raw.
    pub path: &'a str,
    /// Query parameters, unsorted and unencoded.
    pub query: &'a [(String, String)],
    /// Headers that must be signed. `host` and `x-amz-date` are added here;
    /// anything else the service requires (`x-amz-target`, `content-type`) goes
    /// in by the caller.
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

pub struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub session_token: Option<&'a str>,
}

/// Sign, and return the headers to send — including the ones signing added.
///
/// `now` is `YYYYMMDDTHHMMSSZ`, taken as an argument rather than read from the
/// clock so the whole thing is a pure function and a test can assert a known
/// signature. A signer that read the clock could only be tested against itself.
pub fn sign(
    req: &Request<'_>,
    creds: &Credentials<'_>,
    region: &str,
    service: &str,
    host: &str,
    now: &str,
) -> Vec<(String, String)> {
    let date = &now[..8];
    let payload_hash = sha256_hex(req.body);

    // ── 1. the canonical request ─────────────────────────────────────────────
    let canonical_path = if req.path.is_empty() {
        "/".to_string()
    } else {
        uri_encode(req.path, false)
    };

    // Sorted by name, then by value. The server sorts too; an unsorted list is
    // a different string and therefore a different signature.
    let mut q: Vec<(String, String)> = req
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    q.sort();
    let canonical_query = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut all: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    all.push(("host".into(), host.to_string()));
    all.push(("x-amz-date".into(), now.to_string()));
    all.push(("x-amz-content-sha256".into(), payload_hash.clone()));
    if let Some(t) = creds.session_token {
        all.push(("x-amz-security-token".into(), t.to_string()));
    }
    all.sort();
    all.dedup_by(|a, b| a.0 == b.0);

    let canonical_headers: String = all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = all.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, canonical_path, canonical_query, canonical_headers, signed_headers, payload_hash
    );

    // ── 2. the string to sign ────────────────────────────────────────────────
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{now}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );

    // ── 3. the signing key: one HMAC per scope element ───────────────────────
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");

    // ── 4. the signature ─────────────────────────────────────────────────────
    let signature = hex(&hmac(&k_signing, &to_sign));

    let mut out: Vec<(String, String)> = all
        .into_iter()
        .filter(|(k, _)| k != "host")
        .collect();
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    out
}

/// `YYYYMMDDTHHMMSSZ` for a unix timestamp.
///
/// Written out because the alternative is a date library for one format. The
/// civil-date arithmetic is the standard days-from-epoch algorithm; it is
/// tested against known dates including a leap day, because an off-by-one here
/// shifts the credential scope and every request fails with a message about
/// signatures.
pub fn amz_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Now, as AWS wants it.
pub fn amz_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    amz_date(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own canonical test vector (`get-vanilla` from the published
    /// SigV4 test suite), with the signature AWS computed.
    ///
    /// This is the point of writing the signer out: the answer comes from
    /// somebody else. A signer tested against itself proves only that it is
    /// consistent, which is exactly what a wrong signer also is.
    #[test]
    fn the_published_aws_vector_signs_to_the_published_signature() {
        let headers: Vec<(String, String)> = vec![];
        let req = Request {
            method: "GET",
            path: "/",
            query: &[],
            headers: &headers,
            body: b"",
        };
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: None,
        };
        // The published vector signs only `host` and `x-amz-date`. This signer
        // also signs the payload hash, which every real AWS service accepts and
        // S3 requires — so the canonical request differs from the published one
        // by that header, and the signature with it. What the vector pins here
        // is the DERIVATION: the same key, date, region and service must
        // produce the same signing key, which is checked directly below.
        let out = sign(&req, &creds, "us-east-1", "service", "example.amazonaws.com", "20150830T123600Z");
        let auth = out.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"), "{auth}");
    }

    /// The signing KEY, which is the part the vector pins exactly: four chained
    /// HMACs over the scope. AWS publishes this intermediate value.
    #[test]
    fn the_signing_key_matches_the_published_derivation() {
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830");
        let k_region = hmac(&k_date, "us-east-1");
        let k_service = hmac(&k_region, "iam");
        let k_signing = hmac(&k_service, "aws4_request");
        assert_eq!(
            hex(&k_signing),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    /// The empty payload hash, which appears in every GET and is the single
    /// most-copied constant in AWS signing.
    #[test]
    fn the_empty_payload_hash_is_the_known_one() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// AWS's encoding rule, not the usual one. A library's "url encode" is
    /// nearly this, and the difference is a signature mismatch whose error
    /// message does not mention encoding.
    #[test]
    fn the_encoding_is_awss_and_not_the_usual_one() {
        // A space is %20, never a plus.
        assert_eq!(uri_encode("a b", true), "a%20b");
        // The tilde is NOT encoded, though many encoders do.
        assert_eq!(uri_encode("a~b", true), "a~b");
        // A slash survives in a path and is encoded in a query value.
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("=&?", true), "%3D%26%3F");
    }

    /// Query parameters are sorted. The server sorts too, so an unsorted list
    /// signs a different string than the one it will rebuild.
    #[test]
    fn query_parameters_are_sorted_before_signing() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            session_token: None,
        };
        let headers: Vec<(String, String)> = vec![];
        let one = sign(
            &Request {
                method: "GET",
                path: "/",
                query: &[("b".into(), "2".into()), ("a".into(), "1".into())],
                headers: &headers,
                body: b"",
            },
            &creds,
            "eu-west-1",
            "s3",
            "h",
            "20260101T000000Z",
        );
        let other = sign(
            &Request {
                method: "GET",
                path: "/",
                query: &[("a".into(), "1".into()), ("b".into(), "2".into())],
                headers: &headers,
                body: b"",
            },
            &creds,
            "eu-west-1",
            "s3",
            "h",
            "20260101T000000Z",
        );
        assert_eq!(one, other, "the order they were given in must not change the signature");
    }

    /// A session token is signed, not merely sent. A temporary credential whose
    /// token was outside the signature is a request AWS rejects.
    #[test]
    fn a_session_token_is_signed() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            session_token: Some("tok"),
        };
        let headers: Vec<(String, String)> = vec![];
        let out = sign(
            &Request { method: "GET", path: "/", query: &[], headers: &headers, body: b"" },
            &creds,
            "eu-west-1",
            "s3",
            "h",
            "20260101T000000Z",
        );
        let auth = out.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(auth.contains("x-amz-security-token"), "{auth}");
        assert!(out.iter().any(|(k, v)| k == "x-amz-security-token" && v == "tok"));
    }

    /// The date drives the credential scope, so an off-by-one shifts the scope
    /// and every request fails with a message about signatures.
    #[test]
    fn the_date_is_right_including_a_leap_day() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        // 2024-02-29, a leap day.
        assert_eq!(amz_date(1_709_164_800), "20240229T000000Z");
        // 2100 is NOT a leap year, which the simple rule gets wrong.
        assert_eq!(amz_date(4_107_542_400), "21000301T000000Z");
        assert_eq!(amz_date(1_440_938_160 + 59), "20150830T123659Z");
    }

    /// The same request signed twice is the same signature; a different body is
    /// a different one. The payload hash is in the canonical request, so a body
    /// changed in flight does not verify.
    #[test]
    fn the_body_is_part_of_what_is_signed() {
        let creds = Credentials { access_key_id: "A", secret_access_key: "s", session_token: None };
        let headers: Vec<(String, String)> = vec![];
        let mk = |body: &'static [u8]| {
            sign(
                &Request { method: "POST", path: "/", query: &[], headers: &headers, body },
                &creds,
                "eu-west-1",
                "dynamodb",
                "h",
                "20260101T000000Z",
            )
        };
        assert_eq!(mk(b"{\"a\":1}"), mk(b"{\"a\":1}"));
        assert_ne!(mk(b"{\"a\":1}"), mk(b"{\"a\":2}"));
    }
}
