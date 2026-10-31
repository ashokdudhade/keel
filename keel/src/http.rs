//! Minimal HTTP vocabulary shared by the JSON API (`api`) and control-plane
//! (`daemon`) servers, decoupled from the `tiny_http` transport.
//!
//! Routing functions (`api::build_response`, `daemon::dispatch`) speak only
//! in `HttpMethod` and `u16` statuses; the `tiny_http` types (`Request`,
//! `Response`, `Server`) appear solely at each server's accept/respond edge.
//! Replacing the transport means reimplementing the two edges plus
//! `HttpMethod::from_tiny_http` — routing and its tests stay untouched.

use tiny_http::Method;

/// Request methods the Keel servers distinguish. Everything else folds
/// into [`HttpMethod::Other`]: routing only ever branches on GET/POST/DELETE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpMethod {
    /// Read-only queries (API routes, daemon `/health` and `/status`).
    Get,
    /// Mutating control calls (daemon `/watch`).
    Post,
    /// Control teardown (daemon `/watch`).
    Delete,
    /// Any other method; routing treats these as not matched.
    Other,
}

impl HttpMethod {
    /// Adapt a transport method into routing vocabulary.
    pub(crate) fn from_tiny_http(method: &Method) -> Self {
        match method {
            Method::Get => HttpMethod::Get,
            Method::Post => HttpMethod::Post,
            Method::Delete => HttpMethod::Delete,
            _ => HttpMethod::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_known_methods() {
        assert_eq!(HttpMethod::from_tiny_http(&Method::Get), HttpMethod::Get);
        assert_eq!(HttpMethod::from_tiny_http(&Method::Post), HttpMethod::Post);
        assert_eq!(
            HttpMethod::from_tiny_http(&Method::Delete),
            HttpMethod::Delete
        );
    }

    #[test]
    fn folds_unknown_methods() {
        assert_eq!(
            HttpMethod::from_tiny_http(&Method::Patch),
            HttpMethod::Other
        );
        assert_eq!(
            HttpMethod::from_tiny_http(&Method::Options),
            HttpMethod::Other
        );
    }
}
