use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::header;

pub(crate) fn forward_request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    let connection_hops = connection_hop_headers(headers);

    for (name, value) in headers.iter() {
        if should_drop_request_header(name, &connection_hops) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    out
}

pub(crate) fn forward_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    let connection_hops = connection_hop_headers(headers);

    for (name, value) in headers.iter() {
        if should_drop_response_header(name, &connection_hops) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    out
}

fn should_drop_request_header(name: &HeaderName, connection_hops: &[HeaderName]) -> bool {
    if is_hop_by_hop(name, connection_hops) {
        return true;
    }
    if *name == header::AUTHORIZATION {
        return true;
    }
    if *name == header::HOST {
        return true;
    }
    if *name == header::CONTENT_LENGTH {
        return true;
    }

    let name_str = name.as_str();
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

fn should_drop_response_header(name: &HeaderName, connection_hops: &[HeaderName]) -> bool {
    is_hop_by_hop(name, connection_hops)
}

fn is_hop_by_hop(name: &HeaderName, connection_hops: &[HeaderName]) -> bool {
    if connection_hops.iter().any(|n| n == name) {
        return true;
    }

    if name.as_str() == "keep-alive" {
        return true;
    }

    matches!(
        *name,
        header::CONNECTION
            | header::PROXY_AUTHENTICATE
            | header::PROXY_AUTHORIZATION
            | header::TE
            | header::TRAILER
            | header::TRANSFER_ENCODING
            | header::UPGRADE
    )
}

fn connection_hop_headers(headers: &HeaderMap) -> Vec<HeaderName> {
    let mut out = Vec::new();

    for value in headers.get_all(header::CONNECTION).iter() {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for token in text.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let Ok(name) = HeaderName::from_bytes(token.as_bytes()) else {
                continue;
            };
            out.push(name);
        }
    }

    out
}
