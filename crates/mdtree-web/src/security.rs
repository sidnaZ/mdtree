//! Session credential generation and same-origin enforcement.
//!
//! The listener is reachable through every IPv4 interface and is not an
//! authenticated multi-user collaboration server. Each launch creates an
//! unguessable session credential used to
//! authenticate mutating requests, WebSocket setup, and Stop. Requests enforce
//! same-origin policy and reject untrusted origins; no permissive CORS mode is
//! enabled.

use std::fmt::Write as _;

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;

/// Length, in bytes, of the random session credential before hex encoding.
const CREDENTIAL_BYTES: usize = 32;

/// Generates one unguessable per-launch session credential.
#[must_use]
pub(crate) fn generate_session_credential() -> String {
    let mut bytes = [0u8; CREDENTIAL_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Rejects requests whose `Origin` header does not match the requested host.
///
/// Requests without an `Origin` header — ordinary same-origin top-level
/// navigation — are allowed through; only a present, mismatched `Origin` is
/// rejected. There is no permissive CORS mode.
pub(crate) async fn enforce_same_origin(request: Request, next: Next) -> Response {
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let matches = request
            .headers()
            .get(header::HOST)
            .is_some_and(|host| origin_matches_host(host, origin));
        if !matches {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(axum::body::Body::from(
                    "cross-origin requests are not permitted",
                ))
                .expect("static forbidden response is valid");
        }
    }
    next.run(request).await
}

fn origin_matches_host(host: &HeaderValue, origin: &HeaderValue) -> bool {
    let Ok(host) = host.to_str() else {
        return false;
    };
    origin
        .to_str()
        .is_ok_and(|value| value == format!("http://{host}"))
}

#[cfg(test)]
mod tests {
    use super::{generate_session_credential, origin_matches_host};
    use axum::http::HeaderValue;

    #[test]
    fn generated_credentials_are_long_and_distinct_per_call() {
        let first = generate_session_credential();
        let second = generate_session_credential();
        assert_eq!(first.len(), super::CREDENTIAL_BYTES * 2);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn origin_matching_accepts_only_the_requested_host() {
        let host = HeaderValue::from_static("192.0.2.10:4000");
        assert!(origin_matches_host(
            &host,
            &HeaderValue::from_static("http://192.0.2.10:4000")
        ));
        assert!(!origin_matches_host(
            &host,
            &HeaderValue::from_static("http://127.0.0.1:4000")
        ));
        assert!(!origin_matches_host(
            &host,
            &HeaderValue::from_static("http://127.0.0.1:4001")
        ));
        assert!(!origin_matches_host(
            &host,
            &HeaderValue::from_static("http://evil.example")
        ));
    }
}
