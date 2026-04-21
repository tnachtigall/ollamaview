mod backend_action;
mod fetcher;
mod processor;

use std::fs::OpenOptions;
use backend_action::BackendAction;
use fetcher::fetch_html;
use processor::clean_html;
use std::io::{self, Read, Write};
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use reqwest::Client;
use tokio;
use tokio::sync::mpsc;

struct AppState {
    client: Client,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserMessage {
    query: String,
    urls: Vec<String>,
    ollama_instance: String, // currently uses format "http://127.0.0.1:11434" - TODO: sanitize
    main_model: String,
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

#[tokio::main]
async fn main() {

    // debug logger
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let log_path = format!("{}/ollama_host.log", home_dir);

    if let Ok(mut f) = std::fs::File::create(&log_path) {
        let _ = writeln!(f, "Binary successfully launched by browser at {:?}", std::time::Instant::now());
    }

    // http connection
    let state = Arc::new(AppState {
        client: reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Firefox/124.0")
            .build().unwrap()
    });

    let (tx_out, mut rx_out) = mpsc::channel::<BackendAction>(32);
    let (tx_in, mut rx_in) = mpsc::channel::<String>(32);

    // async stdout writer
    tokio::spawn(async move {
        while let Some(reply) = rx_out.recv().await {
            if let Ok(reply_string) = serde_json::to_string(&reply) {
                let reply_bytes = reply_string.as_bytes();
                let out_len_bytes = (reply_bytes.len() as u32).to_ne_bytes();
                
                let mut stdout = io::stdout().lock();

                let _ = stdout.write_all(&out_len_bytes);
                let _ = stdout.write_all(reply_bytes);
                let _ = stdout.flush();
            }
        }
    });

    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut len_bytes = [0u8; 4];
            if stdin.read_exact(&mut len_bytes).is_err() {
                break;
            }

            let len = u32::from_ne_bytes(len_bytes) as usize;
            let mut buffer = vec![0u8; len];

            if stdin.read_exact(&mut buffer).is_err() {
                break;
            }

            if let Ok(raw_json) = String::from_utf8(buffer) {
                // send  raw json over to tokio pipeline
                let _ = tx_in.blocking_send(raw_json);
            }
        }
    });

    // main loop
    while let Some(raw_json) = rx_in.recv().await {

        if let Ok(mut f) = OpenOptions::new().append(true).open(&log_path) {
            let _ = writeln!(f, "Received payload: {}", raw_json);
        }

        let message = match serde_json::from_str::<BrowserMessage>(&raw_json) {
            Ok(msg) => msg,
            Err(e) => {
                if let Ok(mut f) = OpenOptions::new().append(true).open(&log_path) {
                    let _ = writeln!(f, "Failed to deserialize: {}", e);
                }
                continue;
            }
        };

        let ollama_instance = message.ollama_instance;
        let main_model = message.main_model;
        let query = message.query;
        let urls = message.urls;

        let tx_clone = tx_out.clone();
        let current_state = Arc::clone(&state);
        let prompt = ""; // TODO: Implement prompt!

        // Fan out processing
        tokio::spawn(async move {
            let mut fetch_futures = Vec::new();
            for url in urls {
                fetch_futures.push(fetch_html(url, current_state.client.clone()));
            }

            let results = futures::future::join_all(fetch_futures).await;
            let mut context = String::new();

            for result in results {
                if let Ok(raw_html) = result {
                    let clean_article = clean_html(&raw_html);
                    context.push_str("--- SOURCE ---\n");
                    context.push_str(&clean_article);
                    context.push_str("\n\n");
                }
            }

            let ollama_req = OllamaRequest {
                model: main_model,
                prompt: format!("{}\n\nContext: {}\n\nQuery: {}", prompt, context, query),
                stream: false
            };

            let res = current_state.client
                .post(format!("{}/api/generate", ollama_instance))
                .json(&ollama_req)
                .send()
                .await;

            let overview = match res {
                Ok(response) => {
                    if let Ok(ollama_res) = response.json::<OllamaResponse>().await {
                        ollama_res.response
                    } else {
                        "Error: Failed to parse Ollama JSON".to_string()
                    }
                }
                Err(e) => format!("Error contacting Ollama: {}", e),
            };

            let reply = BackendAction::RustReply {
                status: "Success".to_string(),
                received_query: query,
                message: overview,
            };

            let _ = tx_clone.send(reply).await;
        });
    }
}