use readabilityrs::Readability;

pub fn clean_html(html: &str) -> String {

    let readability = match Readability::new(html, None, None) {
        Ok(r) => r,
        Err(_) => return "Error: Failed to initialize parser".to_string(),
    };

    match readability.parse() {
        Some(article) => {
            eprintln!("SUCCESS: Extracted site content!");
            article.text_content.unwrap_or_else(|| "No text found".to_string())
        },
        None => {
            eprintln!("ERROR: Could not parse article");
            "Could not parse content".to_string()
        }
    }
}