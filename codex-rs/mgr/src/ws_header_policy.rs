use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::header;

pub(crate) fn forward_request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();

    for (name, value) in headers {
        if should_drop_request_header(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    out
}

fn should_drop_request_header(name: &HeaderName) -> bool {
    if matches!(
        *name,
        header::AUTHORIZATION
            | header::HOST
            | header::CONTENT_LENGTH
            | header::CONNECTION
            | header::UPGRADE
    ) {
        return true;
    }

    let name_str = name.as_str();
    if matches!(
        name_str,
        "sec-websocket-accept"
            | "sec-websocket-extensions"
            | "sec-websocket-key"
            | "sec-websocket-version"
    ) {
        return true;
    }
    if name_str.starts_with("cf-") {
        return true;
    }
    if name_str == "cdn-loop" {
        return true;
    }
    if name_str.starts_with("x-forwarded-") {
        return true;
    }
    if name_str == "x-real-ip" {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use pretty_assertions::assert_eq;

    #[test]
    fn websocket_header_forwarding_keeps_application_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer gateway"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("localhost"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(
            "sec-websocket-key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("openai-beta", HeaderValue::from_static("realtime=v1"));
        headers.insert("session_id", HeaderValue::from_static("conv_123"));

        let forwarded = forward_request_headers(&headers);

        assert_eq!(forwarded.get(header::AUTHORIZATION), None);
        assert_eq!(forwarded.get(header::HOST), None);
        assert_eq!(forwarded.get(header::CONNECTION), None);
        assert_eq!(forwarded.get(header::UPGRADE), None);
        assert_eq!(forwarded.get("sec-websocket-key"), None);
        assert_eq!(
            forwarded.get("openai-beta"),
            Some(&HeaderValue::from_static("realtime=v1"))
        );
        assert_eq!(
            forwarded.get("session_id"),
            Some(&HeaderValue::from_static("conv_123"))
        );
    }
}
