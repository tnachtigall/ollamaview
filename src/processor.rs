use readabilityrs::Readability;

const MAX_ARTICLE_CHARS: usize = 3500;

pub fn clean_html(html: &str) -> Option<String> {
    let readability = Readability::new(html, None, None).ok()?;
    let article = readability.parse()?;
    let raw_text = article.text_content?;

    // Collapse excessive whitespace and empty lines while keeping paragraph breaks
    let mut normalized = String::with_capacity(raw_text.len().min(MAX_ARTICLE_CHARS));
    for line in raw_text.lines() {
        let trimmed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('\n');
        }
        normalized.push_str(&trimmed);
        if normalized.len() >= MAX_ARTICLE_CHARS {
            break;
        }
    }

    if normalized.is_empty() {
        return None;
    }

    if normalized.len() > MAX_ARTICLE_CHARS {
        let boundary = normalized.floor_char_boundary(MAX_ARTICLE_CHARS);
        normalized.truncate(boundary);
        normalized.push_str("...");
    }

    Some(normalized)
}