use reqwest;
use reqwest::Client;

pub async fn fetch_html(url: String, client: Client) -> Result<String, reqwest::Error> {

    let response = client.get(url).send().await?;
    let body = response.text().await?;

    Ok(body)
}