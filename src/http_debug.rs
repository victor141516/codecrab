use std::fmt::Write as _;

use anyhow::Result;
use reqwest::{Request, StatusCode};

use crate::diagnostics::DebugOutput;

pub(crate) fn request(output: &DebugOutput, request: &Request) -> Result<()> {
    if !output.is_enabled() {
        return Ok(());
    }
    let mut rendered = String::from("\n===== OPENAI REQUEST =====\n");
    writeln!(
        rendered,
        "{} {} {:?}",
        request.method(),
        request.url(),
        request.version()
    )?;
    for (name, value) in request.headers() {
        writeln!(
            rendered,
            "{name}: {}",
            String::from_utf8_lossy(value.as_bytes())
        )?;
    }
    rendered.push('\n');
    if let Some(body) = request.body().and_then(reqwest::Body::as_bytes) {
        writeln!(rendered, "{}", String::from_utf8_lossy(body))?;
    }
    rendered.push_str("===== END OPENAI REQUEST =====\n\n");
    output.write_all(rendered.as_bytes())
}

pub(crate) fn response(
    output: &DebugOutput,
    url: &url::Url,
    version: reqwest::Version,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> Result<()> {
    if !output.is_enabled() {
        return Ok(());
    }
    let mut rendered = String::from("\n===== OPENAI RESPONSE =====\n");
    writeln!(rendered, "{url} {version:?} {status}")?;
    for (name, value) in headers {
        writeln!(
            rendered,
            "{name}: {}",
            String::from_utf8_lossy(value.as_bytes())
        )?;
    }
    writeln!(rendered, "\n{body}")?;
    rendered.push_str("===== END OPENAI RESPONSE =====\n\n");
    output.write_all(rendered.as_bytes())
}

#[cfg(test)]
mod tests {
    use reqwest::{Client, StatusCode};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn request_and_response_are_written_to_the_selected_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("openai.log");
        let output = DebugOutput::file(path.clone());
        let http_request = Client::new()
            .post("https://example.com/v1/responses")
            .bearer_auth("secret")
            .body("{\"prompt\":\"hello\"}")
            .build()
            .unwrap();

        request(&output, &http_request).unwrap();
        response(
            &output,
            http_request.url(),
            reqwest::Version::HTTP_11,
            StatusCode::OK,
            &reqwest::header::HeaderMap::new(),
            "{\"answer\":\"hello\"}",
        )
        .unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("===== OPENAI REQUEST ====="));
        assert!(contents.contains("authorization: Bearer secret"));
        assert!(contents.contains("{\"prompt\":\"hello\"}"));
        assert!(contents.contains("===== OPENAI RESPONSE ====="));
        assert!(contents.contains("{\"answer\":\"hello\"}"));
    }
}
