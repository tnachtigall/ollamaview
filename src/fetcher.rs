use std::time::Duration;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;

const FETCH_TIMEOUT: Duration = Duration::from_millis(3500);
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

fn url_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub async fn fetch_ddg_urls(subquery: &str, client: &Client, limit: usize) -> Vec<String> {
    let base = std::env::var("OLLAMAVIEW_DDG_URL")
        .unwrap_or_else(|_| "https://html.duckduckgo.com/html/".to_string());
    let search_url = format!(
        "{}?q={}",
        base,
        url_encode_component(subquery)
    );

    let Ok(resp) = client
        .get(&search_url)
        .timeout(Duration::from_millis(2200))
        .send()
        .await
    else {
        return Vec::new();
    };

    let Ok(html) = resp.text().await else {
        return Vec::new();
    };

    let mut urls = Vec::new();
    for segment in html.split("uddg=") {
        if let Some(end_idx) = segment.find(|c| c == '&' || c == '"' || c == '\'') {
            let encoded = &segment[..end_idx];
            let decoded = percent_decode(encoded);
            if (decoded.starts_with("http://") || decoded.starts_with("https://"))
                && !decoded.contains("duckduckgo.com")
                && !urls.contains(&decoded)
            {
                urls.push(decoded);
                if urls.len() >= limit {
                    break;
                }
            }
        }
    }
    urls
}