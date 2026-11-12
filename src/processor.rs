use readabilityrs::Readability;
use std::collections::HashSet;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
    "our", "out", "has", "have", "from", "this", "that", "with", "what", "which", "when", "where",
    "who", "why", "how", "into", "more", "other", "some", "such", "than", "then", "them", "these",
    "they", "their", "there", "will", "would", "could", "should", "about", "after", "before",
    "between", "under", "over", "again", "further", "once", "here", "does", "did", "doing", "been",
    "being", "were", "your",
];

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF // Hiragana & Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul Syllables
    )
}

fn extract_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_lowercase();
        let cjk_chars: Vec<char> = lower.chars().filter(|&c| is_cjk(c)).collect();
        if !cjk_chars.is_empty() {
            // Emit individual CJK characters and overlapping bigrams for unspaced CJK text
            for &ch in &cjk_chars {
                tokens.push(ch.to_string());
            }
            for window in cjk_chars.windows(2) {
                let mut bi = String::with_capacity(6);
                bi.push(window[0]);
                bi.push(window[1]);
                tokens.push(bi);
            }
        } else if lower.chars().count() >= 2 {
            tokens.push(lower);
        }
    }
    tokens
}

pub fn tokenize_query(query: &str) -> Vec<String> {
    let stop_set: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut terms = Vec::new();

    for tok in extract_tokens(query) {
        if !stop_set.contains(tok.as_str()) && seen.insert(tok.clone()) {
            terms.push(tok);
        }
    }
    terms
}

fn tokenize_paragraph(text: &str) -> Vec<String> {
    extract_tokens(text)
}

pub fn clean_html(
    html: &str,
    query_terms: &[String],
    max_chars: usize,
    use_bm25: bool,
) -> Option<String> {
    let readability = Readability::new(html, None, None).ok()?;
    let article = readability.parse()?;
    let raw_text = article.text_content?;

    let mut paragraphs = Vec::new();
    let mut total_chars = 0usize;

    for line in raw_text.lines() {
        let trimmed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            continue;
        }
        total_chars += trimmed.len() + 1;
        paragraphs.push(trimmed);
    }

    // Reject thin stubs (e.g. cookie walls or "enable JavaScript" notices) so they don't consume a source slot
    if total_chars < 200 || paragraphs.is_empty() {
        return None;
    }

    // Fast path: if the whole article fits in max_chars, BM25 is disabled, or no query terms exist, return sequentially
    if !use_bm25 || total_chars <= max_chars || paragraphs.len() <= 2 || query_terms.is_empty() {
        let mut out = paragraphs.join("\n");
        if out.len() > max_chars {
            let boundary = out.floor_char_boundary(max_chars);
            out.truncate(boundary);
            out.push_str("...");
        }
        return Some(out);
    }

    // BM25 paragraph ranking (k1 = 1.5, b = 0.75)
    const K1: f64 = 1.5;
    const B: f64 = 0.75;

    let n_docs = paragraphs.len() as f64;
    let tokenized_paras: Vec<Vec<String>> =
        paragraphs.iter().map(|p| tokenize_paragraph(p)).collect();
    let total_tokens: usize = tokenized_paras.iter().map(|t| t.len()).sum();
    let avgdl = (total_tokens as f64 / n_docs).max(1.0);

    // Precompute IDF for each query term across the article's paragraphs
    let idfs: Vec<f64> = query_terms
        .iter()
        .map(|term| {
            let df = tokenized_paras
                .iter()
                .filter(|tokens| tokens.iter().any(|t| t == term))
                .count() as f64;
            ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln()
        })
        .collect();

    // Always keep Paragraph 0 (lead paragraph) for topic grounding
    let mut selected_indices = vec![0usize];
    let mut current_chars = paragraphs[0].len();

    let mut scored_candidates: Vec<(usize, f64)> = (1..paragraphs.len())
        .map(|idx| {
            let tokens = &tokenized_paras[idx];
            let dl = tokens.len() as f64;
            let mut score = 0.0;

            for (t_idx, term) in query_terms.iter().enumerate() {
                let tf = tokens.iter().filter(|&t| t == term).count() as f64;
                if tf > 0.0 {
                    let num = tf * (K1 + 1.0);
                    let denom = tf + K1 * (1.0 - B + B * (dl / avgdl));
                    score += idfs[t_idx] * (num / denom);
                }
            }

            // Slight positional preference for earlier paragraphs on ties
            score += 0.01 / (idx as f64 + 1.0);
            (idx, score)
        })
        .collect();

    scored_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (idx, _) in scored_candidates {
        let p_len = paragraphs[idx].len() + 1;
        if current_chars + p_len > max_chars && selected_indices.len() > 1 {
            continue;
        }
        selected_indices.push(idx);
        current_chars += p_len;
        if current_chars >= max_chars {
            break;
        }
    }

    // Restore original chronological document order
    selected_indices.sort_unstable();

    let mut normalized = String::with_capacity(current_chars.min(max_chars + 16));
    let mut prev_idx: Option<usize> = None;

    for idx in selected_indices {
        if let Some(prev) = prev_idx {
            if idx > prev + 1 {
                normalized.push_str("\n...\n");
            } else {
                normalized.push('\n');
            }
        }
        normalized.push_str(&paragraphs[idx]);
        prev_idx = Some(idx);
    }

    if normalized.len() > max_chars {
        let boundary = normalized.floor_char_boundary(max_chars);
        normalized.truncate(boundary);
        normalized.push_str("...");
    }

    Some(normalized)
}