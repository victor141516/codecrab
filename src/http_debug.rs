use reqwest::{Request, StatusCode};

pub(crate) fn request(enabled: bool, request: &Request) {
    if !enabled {
        return;
    }
    eprintln!("\n===== OPENAI REQUEST =====");
    eprintln!(
        "{} {} {:?}",
        request.method(),
        request.url(),
        request.version()
    );
    for (name, value) in request.headers() {
        eprintln!("{name}: {}", String::from_utf8_lossy(value.as_bytes()));
    }
    eprintln!();
    if let Some(body) = request.body().and_then(reqwest::Body::as_bytes) {
        eprintln!("{}", String::from_utf8_lossy(body));
    }
    eprintln!("===== END OPENAI REQUEST =====\n");
}

pub(crate) fn response(
    enabled: bool,
    url: &url::Url,
    version: reqwest::Version,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) {
    if !enabled {
        return;
    }
    eprintln!("\n===== OPENAI RESPONSE =====");
    eprintln!("{url} {version:?} {status}");
    for (name, value) in headers {
        eprintln!("{name}: {}", String::from_utf8_lossy(value.as_bytes()));
    }
    eprintln!("\n{body}");
    eprintln!("===== END OPENAI RESPONSE =====\n");
}
