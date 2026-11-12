mod backend_action;
mod fetcher;
mod processor;

use backend_action::BackendAction;
use fetcher::fetch_html;
use processor::clean_html;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const SYSTEM_PROMPT: &str = "You are a helpful, privacy-focused search assistant. \
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
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: Option<String>,
    error: Option<String>,
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
        .connect_timeout(Duration::from_secs(4))
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

        let tx_clone = tx_out.clone();
        let current_state = Arc::clone(&state);

        active_task = Some(tokio::spawn(async move {
            // Fetch and clean all scraped URLs concurrently
            let fetch_futures = urls.into_iter().map(|url| {
                let state_ref = Arc::clone(&current_state);
                async move {
                    match fetch_html(&url, &state_ref.client).await {
                        Ok(raw_html) => {
                            let cleaned =
                                tokio::task::spawn_blocking(move || clean_html(&raw_html))
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
            });

            let results = futures::future::join_all(fetch_futures).await;
            let mut context = String::new();
            let mut source_count = 0usize;

            for (url, clean_article) in results.into_iter().flatten() {
                source_count += 1;
                context.push_str(&format!("--- SOURCE {} ({}) ---\n", source_count, url));
                context.push_str(&clean_article);
                context.push_str("\n\n");
            }

            log_msg(
                &current_state.log_path,
                &format!(
                    "Prepared context from {} sources ({} bytes) for query '{}'",
                    source_count,
                    context.len(),
                    query
                ),
            );

            let full_prompt = if context.is_empty() {
                format!(
                    "{}\n\n(Note: No external webpage content could be extracted.)\n\nQuery: {}",
                    SYSTEM_PROMPT, query
                )
            } else {
                format!("{}\n\nContext:\n{}\nQuery: {}", SYSTEM_PROMPT, context, query)
            };

            let ollama_req = OllamaRequest {
                model: main_model,
                prompt: full_prompt,
                stream: false,
            };

            let endpoint = format!("{}/api/generate", ollama_instance);
            let res = current_state
                .client
                .post(&endpoint)
                .json(&ollama_req)
                .send()
                .await;

            let (status_str, overview) = match res {
                Ok(response) => {
                    let http_status = response.status();
                    match response.text().await {
                        Ok(body_text) => {
                            if let Ok(ollama_res) =
                                serde_json::from_str::<OllamaResponse>(&body_text)
                            {
                                if let Some(err) = ollama_res.error {
                                    ("Error".to_string(), format!("Ollama error: {}", err))
                                } else if let Some(ans) = ollama_res.response {
                                    ("Success".to_string(), ans)
                                } else {
                                    (
                                        "Error".to_string(),
                                        format!("Empty response from Ollama (HTTP {})", http_status),
                                    )
                                }
                            } else if !http_status.is_success() {
                                (
                                    "Error".to_string(),
                                    format!("Ollama HTTP {}: {}", http_status, body_text),
                                )
                            } else {
                                (
                                    "Error".to_string(),
                                    "Error: Failed to parse Ollama JSON".to_string(),
                                )
                            }
                        }
                        Err(e) => (
                            "Error".to_string(),
                            format!("Error reading Ollama response: {}", e),
                        ),
                    }
                }
                Err(e) => (
                    "Error".to_string(),
                    format!("Error contacting Ollama at {}: {}", ollama_instance, e),
                ),
            };

            log_msg(
                &current_state.log_path,
                &format!("Sending reply with status '{}' for query '{}'", status_str, query),
            );

            let reply = BackendAction::RustReply {
                status: status_str,
                received_query: query,
                message: overview,
            };

            let _ = tx_clone.send(reply).await;
        }));
    }

    if let Some(task) = active_task {
        let _ = task.await;
    }
    drop(tx_out);
    let _ = stdout_handle.await;
}