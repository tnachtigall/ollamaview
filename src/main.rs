mod backend_action;
mod fetcher;
mod processor;

use backend_action::BackendAction;
use fetcher::fetch_html;
use futures::stream::{FuturesUnordered, StreamExt};
use processor::{clean_html, tokenize_query};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful, privacy-focused search assistant. \
Using the provided web search sources, write a clear, concise, and accurate overview \
that directly answers the user's search query. Format your response in clean Markdown \
(such as bullet points, bold key terms, and short sections where helpful). Synthesize \
key facts from the sources, stay objective, and avoid speculation. If the sources do not \
contain enough information to fully answer the query, briefly state what is missing and \
provide the best helpful answer you can.";

struct AppState {
    client: Client,
    log_path: String,
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

fn log_msg(log_path: &str, msg: &str) {
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

#[tokio::main]
async fn main() {
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let log_path = format!("{}/ollama_host.log", home_dir);

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

        let tx_clone = tx_out.clone();
        let current_state = Arc::clone(&state);

        active_task = Some(tokio::spawn(async move {
            const OVERALL_FETCH_DEADLINE: Duration = Duration::from_millis(2200);

            // Scale source character budgets dynamically with chosen token context window and target_sources
            let max_total_context_chars = ((num_ctx.saturating_sub(1200)) as usize * 3).max(1800);
            let max_article_chars = (max_total_context_chars / target_sources).clamp(700, 12000);
            let query_terms = Arc::new(tokenize_query(&query));

            let mut fetch_stream: FuturesUnordered<_> = urls
                .into_iter()
                .map(|url| {
                    let state_ref = Arc::clone(&current_state);
                    let terms_ref = Arc::clone(&query_terms);
                    async move {
                        match fetch_html(&url, &state_ref.client).await {
                            Ok(raw_html) => {
                                let cleaned = tokio::task::spawn_blocking(move || {
                                    clean_html(&raw_html, &terms_ref, max_article_chars)
                                })
                                .await
                                .ok()
                                .flatten();
                                cleaned.map(|text| (url, text))
                            }
                            Err(err) => {
                                log_msg(&state_ref.log_path, &err);
                                None
                            }
                        }
                    }
                })
                .collect();

            let mut context = String::new();
            let mut source_count = 0usize;
            let overall_deadline = tokio::time::Instant::now() + OVERALL_FETCH_DEADLINE;

            loop {
                match tokio::time::timeout_at(overall_deadline, fetch_stream.next()).await {
                    Ok(Some(Some((url, clean_article)))) => {
                        source_count += 1;
                        context.push_str(&format!("--- SOURCE {} ({}) ---\n", source_count, url));
                        context.push_str(&clean_article);
                        context.push_str("\n\n");

                        // Stop immediately once we reach the requested number of usable sources or total budget
                        if source_count >= target_sources || context.len() >= max_total_context_chars {
                            break;
                        }
                    }
                    Ok(Some(None)) => {
                        // A URL failed or yielded empty text; continue waiting for remaining candidate URLs
                    }
                    Ok(None) | Err(_) => {
                        // All URLs completed or deadline reached — proceed immediately to Ollama
                        break;
                    }
                }
            }

            log_msg(
                &current_state.log_path,
                &format!(
                    "Prepared context from {} sources ({} bytes, num_ctx={}) for query '{}'",
                    source_count,
                    context.len(),
                    num_ctx,
                    query
                ),
            );

            let full_prompt = if context.is_empty() {
                format!(
                    "{}\n\n(Note: No external webpage content could be extracted.)\n\nQuery: {}",
                    system_prompt, query
                )
            } else {
                format!("{}\n\nContext:\n{}\nQuery: {}", system_prompt, context, query)
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

                    let format_display = |think_txt: &str, resp_txt: &str| -> String {
                        if think_txt.trim().is_empty() {
                            resp_txt.to_string()
                        } else {
                            let quoted_think = think_txt
                                .trim()
                                .lines()
                                .map(|l| format!("> {}", l))
                                .collect::<Vec<_>>()
                                .join("\n");
                            if resp_txt.is_empty() {
                                format!("> **Thinking...**\n{}\n", quoted_think)
                            } else {
                                format!("> **Thought Process**\n{}\n\n---\n\n{}", quoted_think, resp_txt)
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
                                                        message: format_display(&thinking_buf, &accumulated),
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

                    let combined = format_display(&thinking_buf, &accumulated);
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