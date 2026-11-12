use std::time::Duration;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;

const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

pub async fn fetch_html(url: &str, client: &Client) -> Result<String, String> {
    let response = client
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

    response
        .text()
        .await
        .map_err(|e| format!("Failed reading body from {}: {}", url, e))
}