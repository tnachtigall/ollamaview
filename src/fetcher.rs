use std::time::Duration;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;

const FETCH_TIMEOUT: Duration = Duration::from_millis(2500);
const MAX_HTML_BYTES: usize = 256 * 1024; // 256 KB cap for fast download & DOM parsing

pub async fn fetch_html(url: &str, client: &Client) -> Result<String, String> {
    let mut response = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Request failed for {}: {}", url, e))?
        .error_for_status()
        .map_err(|e| format!("HTTP error for {}: {}", url, e))?;

    if let Some(content_type) = response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        let ct_lower = content_type.to_ascii_lowercase();
        if !ct_lower.contains("text/html") && !ct_lower.contains("application/xhtml+xml") {
            return Err(format!("Skipped non-HTML content ({}) from {}", content_type, url));
        }
    }

    let mut buf = Vec::with_capacity(64 * 1024);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Failed reading chunk from {}: {}", url, e))?
    {
        let remaining = MAX_HTML_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= MAX_HTML_BYTES {
            break;
        }
    }

    Ok(String::from_utf8_lossy(&buf).into_owned())
}