mod backend_action;
mod fetcher;
mod processor;

use backend_action::BackendAction;
use fetcher::{fetch_ddg_urls, fetch_html};
use futures::stream::{FuturesUnordered, StreamExt};
use processor::{clean_html, tokenize_query};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a precise, privacy-focused search synthesis engine. \
Your task is to generate a dense, objective overview that directly answers the user's query using ONLY the provided \"--- SOURCE N ---\" search excerpts.\n\n\
### Core Rules\n\
1. **Strict Grounding:** Base every statement exclusively on the provided sources. Never rely on prior training data, assume unstated facts, or extrapolate to fill gaps.\n\
2. **Preserve Granularity:** Retain concrete details from the sources—such as exact numbers, units, configuration flags, function/API names, dates, versions, and proper nouns—rather than summarizing them into vague generalizations.\n\
3. **Calibrated Claims & Contradictions:** Mirror the exact certainty of the sources. Avoid absolute language if sources describe probabilities, trade-offs, or conditions. Explicitly report conflicting viewpoints or version differences neutrally if sources disagree.\n\
4. **High Information Density:** Start directly with a 1–2 sentence core answer or definition. Eliminate introductory filler (e.g., \"Based on the provided sources...\"), transitional prose, and closing meta-commentary.\n\
5. **Formatting:** Use clean Markdown: short sections, inline code spans for technical identifiers, and bullet points starting with **Bold Key Terms:** followed by concise facts.\n\
6. **Strict Abstention:** If the provided sources do not contain enough information to answer the query or a part of it, briefly state what is missing and stop. Do not speculate or fall back on general knowledge.";

struct AppState {
    client: Client,
    log_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserMessage {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    query: String,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    ollama_instance: String,
    #[serde(default)]
    main_model: String,
    #[serde(default)]
    system_prompt: String,
    #[serde(default)]
    max_sources: Option<usize>,
    #[serde(default)]
    context_size: Option<u32>,
    #[serde(default)]
    thinking_mode: bool,
    #[serde(default)]
    query_expansion: bool,
    #[serde(default)]
    helper_model: String,
    #[serde(default)]
    response_language: String,
    #[serde(default)]
    bm25_enabled: Option<bool>,
    #[serde(default)]
    include_citations: Option<bool>,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_ctx: u32,
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    think: bool,
    keep_alive: &'static str,
    options: OllamaOptions,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: Option<String>,
    thinking: Option<String>,
    error: Option<String>,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaModelInfo>,
}

#[derive(Deserialize)]
struct OllamaModelInfo {
    name: String,
}

fn default_log_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home)
                .join("Library")
                .join("Logs")
                .join("ollamaview")
                .join("ollama_host.log");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .filter(|v| !v.is_empty())
        {
            return PathBuf::from(local_app_data)
                .join("ollamaview")
                .join("logs")
                .join("ollama_host.log");
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(state_home)
                .join("ollamaview")
                .join("ollama_host.log");
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("ollamaview")
                .join("ollama_host.log");
        }
    }

    std::env::temp_dir().join("ollamaview").join("ollama_host.log")
}

fn log_msg(log_path: &Path, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{}", msg);
    }
}

fn sanitize_ollama_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "http://127.0.0.1:11434".to_string();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    }
}

fn markdown_safe_url(url: &str) -> String {
    url.trim()
        .replace('(', "%28")
        .replace(')', "%29")
        .replace(' ', "%20")
}

fn short_source_label(url: &str) -> String {
    let stripped = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_end_matches('/');
    let char_count = stripped.chars().count();
    if char_count <= 58 {
        stripped.to_string()
    } else {
        let head: String = stripped.chars().take(55).collect();
        format!("{}…", head)
    }
}

fn parse_citation_numbers(inner: &str, require_source_prefix: bool, max_source: usize) -> Option<Vec<usize>> {
    let trimmed = inner.trim();
    if trimmed.is_empty() || max_source == 0 {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_top_prefix = lower.starts_with("source ")
        || lower.starts_with("sources ")
        || lower.starts_with("source:")
        || lower.starts_with("sources:");

    if require_source_prefix && !has_top_prefix {
        return None;
    }

    let mut nums = Vec::new();
    for raw_part in trimmed.split(|c| c == ',' || c == ';') {
        let mut part = raw_part.trim();
        if part.is_empty() {
            return None;
        }
        let part_lower = part.to_ascii_lowercase();
        for prefix in ["sources:", "source:", "sources", "source", "#"] {
            if part_lower.starts_with(prefix) {
                part = part[prefix.len()..].trim();
                break;
            }
        }
        if part.is_empty() {
            return None;
        }

        if let Some((start_s, end_s)) = part.split_once('-').or_else(|| part.split_once('–')) {
            let start_n: usize = start_s.trim().parse().ok()?;
            let end_n: usize = end_s.trim().parse().ok()?;
            if start_n == 0 || end_n < start_n || end_n - start_n > 10 {
                return None;
            }
            for k in start_n..=end_n {
                if k >= 1 && k <= max_source && !nums.contains(&k) {
                    nums.push(k);
                }
            }
        } else {
            let k: usize = part.parse().ok()?;
            if k == 0 {
                return None;
            }
            if k <= max_source && !nums.contains(&k) {
                nums.push(k);
            }
        }
    }

    if nums.is_empty() {
        None
    } else {
        Some(nums)
    }
}

fn linkify_line_citations(line: &str, safe_urls: &[String]) -> String {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(line.len() + 64);
    let mut i = 0;
    let mut in_inline_code = false;

    while i < len {
        let ch = chars[i];
        if ch == '`' {
            in_inline_code = !in_inline_code;
            out.push(ch);
            i += 1;
            continue;
        }

        if !in_inline_code && (ch == '[' || ch == '(') {
            let prev_is_ident = if i > 0 {
                let p = chars[i - 1];
                p.is_alphanumeric() || p == '_' || p == '!'
            } else {
                false
            };

            let close_ch = if ch == '[' { ']' } else { ')' };
            if !prev_is_ident {
                let mut close_idx = None;
                for j in (i + 1)..len.min(i + 48) {
                    if chars[j] == close_ch {
                        close_idx = Some(j);
                        break;
                    }
                    if chars[j] == '[' || chars[j] == '(' || chars[j] == '`' {
                        break;
                    }
                }

                if let Some(j) = close_idx {
                    let followed_by_paren = j + 1 < len && chars[j + 1] == '(';
                    if !followed_by_paren {
                        let inner: String = chars[(i + 1)..j].iter().collect();
                        let require_prefix = ch == '(';
                        if let Some(indices) = parse_citation_numbers(&inner, require_prefix, safe_urls.len()) {
                            for (idx_pos, k) in indices.iter().enumerate() {
                                if idx_pos > 0 {
                                    out.push(' ');
                                }
                                out.push_str(&format!("[{}]({})", k, safe_urls[k - 1]));
                            }
                            i = j + 1;
                            continue;
                        }
                    }
                }
            }
        }

        out.push(ch);
        i += 1;
    }

    out
}

fn format_with_citations(text: &str, sources: &[String], append_footer: bool) -> String {
    if sources.is_empty() || text.trim().is_empty() {
        return text.to_string();
    }

    let safe_urls: Vec<String> = sources.iter().map(|u| markdown_safe_url(u)).collect();
    let mut out_lines = Vec::new();
    let mut in_fenced_code = false;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fenced_code = !in_fenced_code;
            out_lines.push(line.to_string());
        } else if in_fenced_code {
            out_lines.push(line.to_string());
        } else {
            out_lines.push(linkify_line_citations(line, &safe_urls));
        }
    }

    let mut result = out_lines.join("\n");
    if append_footer {
        result.push_str("\n\n---\n");
        for (idx, raw_url) in sources.iter().enumerate() {
            let num = idx + 1;
            let safe_url = &safe_urls[idx];
            let label = short_source_label(raw_url);
            result.push_str(&format!(
                "- [{}]({}) [{}]({})\n",
                num, safe_url, label, safe_url
            ));
        }
    }

    result
}

#[tokio::main]
async fn main() {
    let log_path = default_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut f) = std::fs::File::create(&log_path) {
        let _ = writeln!(
            f,
            "Binary successfully launched by browser at {:?}",
            std::time::Instant::now()
        );
    }

    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .connect_timeout(Duration::from_millis(1800))
        .pool_max_idle_per_host(8)
        .tcp_nodelay(true)
        .build()
        .expect("Failed to initialize HTTP client");

    let state = Arc::new(AppState {
        client,
        log_path: log_path.clone(),
    });

    let (tx_out, mut rx_out) = mpsc::channel::<BackendAction>(32);
    let (tx_in, mut rx_in) = mpsc::channel::<String>(32);

    // Async stdout writer for Native Messaging length-prefixed frames
    let log_out = log_path.clone();
    let stdout_handle = tokio::spawn(async move {
        while let Some(reply) = rx_out.recv().await {
            if let Ok(reply_string) = serde_json::to_string(&reply) {
                let reply_bytes = reply_string.as_bytes();
                let out_len_bytes = (reply_bytes.len() as u32).to_ne_bytes();

                let write_res = {
                    let mut stdout = io::stdout().lock();
                    stdout
                        .write_all(&out_len_bytes)
                        .and_then(|_| stdout.write_all(reply_bytes))
                        .and_then(|_| stdout.flush())
                };

                if let Err(e) = write_res {
                    log_msg(&log_out, &format!("Failed writing to stdout: {}", e));
                    break;
                }
            }
        }
    });

    // Dedicated blocking thread for Native Messaging stdin reads
    let log_in = log_path.clone();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut len_bytes = [0u8; 4];
            match stdin.read_exact(&mut len_bytes) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                    log_msg(&log_in, "Browser closed stdin pipe (EOF). Shutting down.");
                    break;
                }
                Err(e) => {
                    log_msg(&log_in, &format!("Failed reading message header: {}", e));
                    break;
                }
            }

            let len = u32::from_ne_bytes(len_bytes) as usize;
            if len == 0 || len > 10 * 1024 * 1024 {
                log_msg(&log_in, &format!("Invalid incoming message length: {}", len));
                break;
            }

            let mut buffer = vec![0u8; len];
            if let Err(e) = stdin.read_exact(&mut buffer) {
                log_msg(&log_in, &format!("Failed reading message payload: {}", e));
                break;
            }

            match String::from_utf8(buffer) {
                Ok(raw_json) => {
                    if tx_in.blocking_send(raw_json).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log_msg(&log_in, &format!("Invalid UTF-8 payload: {}", e));
                }
            }
        }
    });

    let mut active_task: Option<JoinHandle<()>> = None;

    // Main message loop
    while let Some(raw_json) = rx_in.recv().await {
        log_msg(&log_path, &format!("Received payload: {}", raw_json));

        let message = match serde_json::from_str::<BrowserMessage>(&raw_json) {
            Ok(msg) => msg,
            Err(e) => {
                log_msg(&log_path, &format!("Failed to deserialize: {}", e));
                let _ = tx_out
                    .send(BackendAction::RustReply {
                        status: "Error".to_string(),
                        received_query: String::new(),
                        message: format!("Error parsing request in backend: {}", e),
                    })
                    .await;
                continue;
            }
        };

        let ollama_instance = sanitize_ollama_url(&message.ollama_instance);

        if message.action.as_deref() == Some("GET_MODELS") {
            let tx_clone = tx_out.clone();
            let current_state = Arc::clone(&state);
            tokio::spawn(async move {
                let tags_url = format!("{}/api/tags", ollama_instance);
                let reply = match current_state
                    .client
                    .get(&tags_url)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            match response.json::<OllamaTagsResponse>().await {
                                Ok(tags) => {
                                    let models: Vec<String> =
                                        tags.models.into_iter().map(|m| m.name).collect();
                                    let msg = if models.is_empty() {
                                        "Connected to Ollama, but no models are installed.".to_string()
                                    } else {
                                        format!("Found {} model(s).", models.len())
                                    };
                                    BackendAction::ModelsList {
                                        status: "Success".to_string(),
                                        models,
                                        message: msg,
                                    }
                                }
                                Err(e) => BackendAction::ModelsList {
                                    status: "Error".to_string(),
                                    models: Vec::new(),
                                    message: format!("Invalid JSON from Ollama /api/tags: {}", e),
                                },
                            }
                        } else {
                            BackendAction::ModelsList {
                                status: "Error".to_string(),
                                models: Vec::new(),
                                message: format!("Ollama returned HTTP {}", status),
                            }
                        }
                    }
                    Err(e) => BackendAction::ModelsList {
                        status: "Error".to_string(),
                        models: Vec::new(),
                        message: format!("Could not connect to {}: {}", ollama_instance, e),
                    },
                };
                let _ = tx_clone.send(reply).await;
            });
            continue;
        }

        // Cancel any previous in-flight search so only the latest query runs
        if let Some(prev) = active_task.take() {
            prev.abort();
        }

        let main_model = message.main_model;
        let query = message.query;
        let urls = message.urls;
        let system_prompt = if message.system_prompt.trim().is_empty() {
            DEFAULT_SYSTEM_PROMPT.to_string()
        } else {
            message.system_prompt.trim().to_string()
        };
        let target_sources = message.max_sources.unwrap_or(5).clamp(1, 8);
        let num_ctx = message.context_size.unwrap_or(4096).clamp(1024, 32768);
        let thinking_mode = message.thinking_mode;
        let query_expansion = message.query_expansion;
        let helper_model = message.helper_model.trim().to_string();
        let response_language = message.response_language.trim().to_string();
        let use_bm25 = message.bm25_enabled.unwrap_or(true);
        let include_citations = message.include_citations.unwrap_or(true);

        let tx_clone = tx_out.clone();
        let current_state = Arc::clone(&state);

        active_task = Some(tokio::spawn(async move {
            const OVERALL_FETCH_DEADLINE: Duration = Duration::from_millis(3400);
            let task_start = Instant::now();

            // Scale source character budgets dynamically with chosen token context window and target_sources
            let max_total_context_chars = ((num_ctx.saturating_sub(1200)) as usize * 3).max(1800);
            let max_article_chars = (max_total_context_chars / target_sources).clamp(700, 12000);
            let shared_query_terms = Arc::new(RwLock::new(tokenize_query(&query)));

            let mut seen_urls: HashSet<String> = urls.iter().cloned().collect();
            let mut fetch_stream: FuturesUnordered<
                std::pin::Pin<Box<dyn std::future::Future<Output = Option<(String, String, bool)>> + Send>>,
            > = FuturesUnordered::new();

            for url in urls {
                let state_ref = Arc::clone(&current_state);
                fetch_stream.push(Box::pin(async move {
                    match fetch_html(&url, &state_ref.client).await {
                        Ok(raw_html) => {
                            let has_content = tokio::task::spawn_blocking(move || {
                                if clean_html(&raw_html, &[], 250, false).is_some() {
                                    Some(raw_html)
                                } else {
                                    None
                                }
                            })
                            .await
                            .ok()
                            .flatten();
                            has_content.map(|html| (url, html, false))
                        }
                        Err(err) => {
                            log_msg(&state_ref.log_path, &err);
                            None
                        }
                    }
                }));
            }

            // Optional parallel Stage-1 helper model for query expansion
            let mut expansion_rx = if query_expansion && !helper_model.is_empty() {
                let (exp_tx, exp_rx) = mpsc::channel::<Vec<String>>(1);
                let state_exp = Arc::clone(&current_state);
                let terms_exp = Arc::clone(&shared_query_terms);
                let ollama_exp = ollama_instance.clone();
                let query_exp = query.clone();
                let model_exp = helper_model.clone();

                tokio::spawn(async move {
                    let exp_prompt = format!(
                        "You are an orthogonal search query facet expander. Given the user query: \"{}\"\n\
                        Generate 2 web search queries that preserve the exact technical depth and complexity of the original query while expanding it along distinct, orthogonal facets:\n\
                        - Facet 1: Target concrete specifications, configuration parameters, exact API/flag names, or underlying mechanisms.\n\
                        - Facet 2: Target practical edge cases, trade-offs, troubleshooting root causes, or empirical comparisons/benchmarks.\n\
                        Rules:\n\
                        - Retain the core entities and technical specificity of the original query.\n\
                        - Do NOT simplify, generalize, or turn the query into a broad introductory explainer.\n\
                        - Keep the same language as the original query.\n\
                        - Output ONLY the 2 search queries, one per line, with no numbering, labels, bullets, or quotes.",
                        query_exp
                    );
                    let req = OllamaRequest {
                        model: model_exp.clone(),
                        prompt: exp_prompt,
                        stream: false,
                        think: false,
                        keep_alive: "15m",
                        options: OllamaOptions { num_ctx: 1024 },
                    };
                    let endpoint = format!("{}/api/generate", ollama_exp);
                    let Ok(Ok(resp)) = tokio::time::timeout(
                        Duration::from_millis(2400),
                        state_exp.client.post(&endpoint).json(&req).send(),
                    )
                    .await
                    else {
                        log_msg(&state_exp.log_path, "Helper model query expansion timed out or failed");
                        return;
                    };

                    let Ok(ollama_res) = resp.json::<OllamaResponse>().await else {
                        return;
                    };
                    let Some(text) = ollama_res.response else {
                        return;
                    };

                    let subqueries: Vec<String> = text
                        .lines()
                        .map(|l| {
                            let mut s = l.trim().trim_matches(|c: char| c == '-' || c == '*' || c == '"' || c == '\'').trim();
                            // Strip optional leading numbering or facet labels like "1.", "2)", "Facet 1:", "Query 1:"
                            if let Some(rest) = s.strip_prefix("1.").or_else(|| s.strip_prefix("2.")).or_else(|| s.strip_prefix("1)")).or_else(|| s.strip_prefix("2)")) {
                                s = rest.trim();
                            }
                            let lower = s.to_ascii_lowercase();
                            for prefix in ["facet 1:", "facet 2:", "query 1:", "query 2:"] {
                                if lower.starts_with(prefix) {
                                    s = s[prefix.len()..].trim();
                                    break;
                                }
                            }
                            s.trim_matches(|c: char| c == '"' || c == '\'').trim().to_string()
                        })
                        .filter(|l| l.len() >= 3)
                        .take(2)
                        .collect();

                    if subqueries.is_empty() {
                        return;
                    }

                    log_msg(
                        &state_exp.log_path,
                        &format!("Helper model '{}' expanded subqueries: {:?}", model_exp, subqueries),
                    );

                    // Enrich BM25 scoring vocabulary with expanded query terms
                    if let Ok(mut guard) = terms_exp.write() {
                        for sq in &subqueries {
                            for tok in tokenize_query(sq) {
                                if !guard.contains(&tok) {
                                    guard.push(tok);
                                }
                            }
                        }
                    }

                    let ddg_futs = subqueries
                        .iter()
                        .map(|sq| fetch_ddg_urls(sq, &state_exp.client, 2));
                    let extra_batches = futures::future::join_all(ddg_futs).await;
                    let extra_urls: Vec<String> = extra_batches.into_iter().flatten().collect();
                    let _ = exp_tx.send(extra_urls).await;
                });
                Some(exp_rx)
            } else {
                None
            };

            let mut serp_raw_pages: Vec<(String, String)> = Vec::new();
            let mut expanded_raw_pages: Vec<(String, String)> = Vec::new();
            let mut pending_expanded_fetches = 0usize;
            let overall_deadline = tokio::time::Instant::now() + OVERALL_FETCH_DEADLINE;

            loop {
                tokio::select! {
                    biased;
                    exp_msg = async {
                        match expansion_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        expansion_rx = None;
                        if let Some(extra_urls) = exp_msg {
                            for url in extra_urls {
                                if seen_urls.insert(url.clone()) {
                                    pending_expanded_fetches += 1;
                                    let state_ref = Arc::clone(&current_state);
                                    fetch_stream.push(Box::pin(async move {
                                        match fetch_html(&url, &state_ref.client).await {
                                            Ok(raw_html) => {
                                                let has_content = tokio::task::spawn_blocking(move || {
                                                    if clean_html(&raw_html, &[], 250, false).is_some() {
                                                        Some(raw_html)
                                                    } else {
                                                        None
                                                    }
                                                })
                                                .await
                                                .ok()
                                                .flatten();
                                                has_content.map(|html| (url, html, true))
                                            }
                                            Err(err) => {
                                                log_msg(&state_ref.log_path, &err);
                                                None
                                            }
                                        }
                                    }));
                                }
                            }
                        }
                        if pending_expanded_fetches == 0 && serp_raw_pages.len() >= target_sources {
                            break;
                        }
                    }
                    fetch_res = tokio::time::timeout_at(overall_deadline, fetch_stream.next()) => {
                        match fetch_res {
                            Ok(Some(Some((url, raw_html, is_expanded)))) => {
                                if is_expanded {
                                    pending_expanded_fetches = pending_expanded_fetches.saturating_sub(1);
                                    expanded_raw_pages.push((url, raw_html));
                                } else {
                                    serp_raw_pages.push((url, raw_html));
                                }

                                if expansion_rx.is_none() {
                                    let want_expanded = if target_sources >= 2 && (pending_expanded_fetches > 0 || !expanded_raw_pages.is_empty()) {
                                        1
                                    } else {
                                        0
                                    };
                                    let have_expanded = expanded_raw_pages.len() >= want_expanded;
                                    let total_have = serp_raw_pages.len() + expanded_raw_pages.len();
                                    if have_expanded && total_have >= target_sources {
                                        break;
                                    }
                                }
                            }
                            Ok(Some(None)) => {
                                // A candidate URL failed or was a cookie/JS stub; continue waiting for remaining URLs
                            }
                            Ok(None) => {
                                if expansion_rx.is_none() {
                                    break;
                                }
                            }
                            Err(_) => {
                                // Overall fetch deadline reached — proceed immediately to Ollama
                                break;
                            }
                        }
                    }
                }
            }

            // Combine up to (target_sources - 1) SERP pages + 1 expanded page (if available), then clean & rank with enriched BM25 terms
            let mut selected_raw: Vec<(String, String)> = Vec::with_capacity(target_sources);
            let reserve_expanded = if target_sources >= 2 && !expanded_raw_pages.is_empty() { 1 } else { 0 };
            let serp_take = (target_sources - reserve_expanded).min(serp_raw_pages.len());
            selected_raw.extend(serp_raw_pages.into_iter().take(serp_take));
            for exp_page in expanded_raw_pages {
                if selected_raw.len() < target_sources {
                    selected_raw.push(exp_page);
                }
            }

            let final_query_terms = shared_query_terms
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();

            let mut context = String::new();
            let mut used_sources: Vec<String> = Vec::new();
            let mut source_count = 0usize;
            for (url, raw_html) in selected_raw {
                let terms_clone = final_query_terms.clone();
                let cleaned = tokio::task::spawn_blocking(move || {
                    clean_html(&raw_html, &terms_clone, max_article_chars, use_bm25)
                })
                .await
                .ok()
                .flatten();

                if let Some(clean_article) = cleaned {
                    source_count += 1;
                    context.push_str(&format!("--- SOURCE {} ({}) ---\n", source_count, url));
                    context.push_str(&clean_article);
                    context.push_str("\n\n");
                    used_sources.push(url);
                    if source_count >= target_sources || context.len() >= max_total_context_chars {
                        break;
                    }
                }
            }

            let fetch_ms = task_start.elapsed().as_millis();
            log_msg(
                &current_state.log_path,
                &format!(
                    "Prepared context from {} sources ({} bytes, num_ctx={}, bm25={}, fetch_ms={}) for query '{}'",
                    source_count,
                    context.len(),
                    num_ctx,
                    use_bm25,
                    fetch_ms,
                    query
                ),
            );

            let lang_directive = if response_language.is_empty() || response_language.eq_ignore_ascii_case("auto") {
                "IMPORTANT: Write your entire response in the same language as the user's search query.".to_string()
            } else {
                format!(
                    "IMPORTANT: Write your entire response in {}, regardless of the language of the sources or query.",
                    response_language
                )
            };

            let citation_directive = if include_citations && !used_sources.is_empty() {
                "\nIMPORTANT: Append inline numeric citations (e.g., [1], [1, 2]) ONLY at the end of important bullet points and paragraphs to attribute key claims to the numbered --- SOURCE N --- sections in the Context. Do NOT place citations after every sentence within a paragraph, as that hurts readability. Never cite source numbers that do not exist in the Context."
            } else {
                ""
            };

            let full_prompt = if context.is_empty() {
                format!(
                    "{}\n{}\n\n(Note: No external webpage content could be extracted.)\n\nQuery: {}",
                    system_prompt, lang_directive, query
                )
            } else {
                format!(
                    "{}\n{}{}\n\nContext:\n{}\nQuery: {}",
                    system_prompt, lang_directive, citation_directive, context, query
                )
            };

            let ollama_req = OllamaRequest {
                model: main_model,
                prompt: full_prompt,
                stream: true,
                think: thinking_mode,
                keep_alive: "15m",
                options: OllamaOptions { num_ctx },
            };

            let endpoint = format!("{}/api/generate", ollama_instance);
            let res = current_state
                .client
                .post(&endpoint)
                .json(&ollama_req)
                .send()
                .await;

            match res {
                Ok(mut response) => {
                    let http_status = response.status();
                    if !http_status.is_success() {
                        let body_text = response.text().await.unwrap_or_default();
                        let err_msg = serde_json::from_str::<OllamaResponse>(&body_text)
                            .ok()
                            .and_then(|r| r.error)
                            .map(|e| format!("Ollama error: {}", e))
                            .unwrap_or_else(|| format!("Ollama HTTP {}: {}", http_status, body_text));

                        let _ = tx_clone
                            .send(BackendAction::RustReply {
                                status: "Error".to_string(),
                                received_query: query,
                                message: err_msg,
                            })
                            .await;
                        return;
                    }

                    let mut accumulated = String::new();
                    let mut thinking_buf = String::new();
                    let mut line_buf = Vec::new();
                    let mut sent_first_token = false;
                    let mut last_send = Instant::now();
                    let mut stream_err: Option<String> = None;

                    let format_display = |think_txt: &str, resp_txt: &str, is_final: bool| -> String {
                        let rendered_resp = if include_citations && !used_sources.is_empty() {
                            format_with_citations(resp_txt, &used_sources, is_final)
                        } else {
                            resp_txt.to_string()
                        };

                        if think_txt.trim().is_empty() {
                            rendered_resp
                        } else {
                            let quoted_think = think_txt
                                .trim()
                                .lines()
                                .map(|l| format!("> {}", l))
                                .collect::<Vec<_>>()
                                .join("\n");
                            if rendered_resp.is_empty() {
                                format!("> **Thinking...**\n{}\n", quoted_think)
                            } else {
                                format!("> **Thought Process**\n{}\n\n---\n\n{}", quoted_think, rendered_resp)
                            }
                        }
                    };

                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                line_buf.extend_from_slice(&chunk);
                                while let Some(newline_pos) =
                                    line_buf.iter().position(|&b| b == b'\n')
                                {
                                    let line_bytes: Vec<u8> =
                                        line_buf.drain(..=newline_pos).collect();
                                    let line_str = String::from_utf8_lossy(&line_bytes);
                                    let trimmed = line_str.trim();
                                    if trimmed.is_empty() {
                                        continue;
                                    }

                                    if let Ok(part) =
                                        serde_json::from_str::<OllamaResponse>(trimmed)
                                    {
                                        if let Some(err) = part.error {
                                            stream_err = Some(format!("Ollama error: {}", err));
                                            break;
                                        }
                                        let mut updated = false;
                                        if thinking_mode {
                                            if let Some(think_tok) = part.thinking {
                                                if !think_tok.is_empty() {
                                                    thinking_buf.push_str(&think_tok);
                                                    updated = true;
                                                }
                                            }
                                        }
                                        if let Some(token) = part.response {
                                            if !token.is_empty() {
                                                accumulated.push_str(&token);
                                                updated = true;
                                            }
                                        }
                                        if updated {
                                            let should_flush = !sent_first_token
                                                || part.done
                                                || last_send.elapsed()
                                                    >= Duration::from_millis(40);
                                            if should_flush {
                                                sent_first_token = true;
                                                last_send = Instant::now();
                                                let _ = tx_clone
                                                    .send(BackendAction::RustReply {
                                                        status: "Streaming".to_string(),
                                                        received_query: query.clone(),
                                                        message: format_display(&thinking_buf, &accumulated, false),
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                }
                                if stream_err.is_some() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                stream_err = Some(format!("Error reading Ollama stream: {}", e));
                                break;
                            }
                        }
                    }

                    let combined = format_display(&thinking_buf, &accumulated, true);
                    let (final_status, final_message) = if let Some(err) = stream_err {
                        (
                            "Error".to_string(),
                            if combined.is_empty() {
                                err
                            } else {
                                format!("{}\n\n[{}]", combined, err)
                            },
                        )
                    } else if combined.is_empty() {
                        (
                            "Error".to_string(),
                            "Empty response from Ollama.".to_string(),
                        )
                    } else {
                        ("Success".to_string(), combined)
                    };

                    let _ = tx_clone
                        .send(BackendAction::RustReply {
                            status: final_status,
                            received_query: query,
                            message: final_message,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx_clone
                        .send(BackendAction::RustReply {
                            status: "Error".to_string(),
                            received_query: query,
                            message: format!("Error contacting Ollama at {}: {}", ollama_instance, e),
                        })
                        .await;
                }
            }
        }));
    }

    if let Some(task) = active_task {
        let _ = task.await;
    }
    drop(tx_out);
    let _ = stdout_handle.await;
}