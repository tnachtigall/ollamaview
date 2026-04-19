use reqwest;

pub async fn fetch_html(url: String) -> Result<String, reqwest::Error> {
    let client  = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Firefox/124.0")
        .build()?;
    let response = client.get(url).send().await?;
    let body = response.text().await?;

    Ok(body)
}