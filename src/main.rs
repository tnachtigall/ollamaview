mod backend_action;
mod fetcher;
mod processor;

use backend_action::BackendAction;
use fetcher::fetch_html;
use processor::clean_html;
use std::io::{self, ErrorKind, Read, Write};
use serde::{Deserialize};
use tokio;

fn main() {

    loop {
        // get 4 byte header from browser
        let mut len_bytes = [0u8; 4];

        match io::stdin().read_exact(&mut len_bytes) {
            Ok(_) => {}, // Successfully read 4 bytes, continue
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                eprintln!("Disconnected: Browser closed the pipe (EOF). Shutting down.");
                break;
            },
            Err(e) => {
                eprintln!("FATAL: Unexpected read error: {}", e);
                break;
            }
        }

        // get raw json and deserialize
        let len = u32::from_ne_bytes(len_bytes) as usize;
        eprintln!("Success: Browser says incoming JSON is {} bytes long", len);

        let mut buffer = vec![0u8; len];
        if let Err(e) = io::stdin().read_exact(&mut buffer) {
            eprintln!("FATAL: Failed to read the JSON payload. Error: {}", e);
            break;
        }

        let raw_json = String::from_utf8_lossy(&buffer);
        eprintln!("Received JSON: {}", raw_json);

        #[derive(Deserialize)]
        struct BrowserMessage {
            query: String,
            urls: Vec<String>,
        }

        let Ok(message) = serde_json::from_str::<BrowserMessage>(&raw_json) else {
            eprintln!("FATAL: Error while deserializing json query!");
            break;
        };

        // async processing pipeline
        tokio::spawn(async move {
            // build vec with urls (fan out)
            let mut fetch_futures = Vec::new();
            for url in message.urls {
                fetch_futures.push(fetch_html(url));
            }

            // create and fire background tasks for collected urls (fan in)
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
        });

        // reply to browser
        let reply = BackendAction::RustReply {
            status: "Success".to_string(),
            received_query: message.query.clone(),
            message: "The Rust Native Messaging bridge is fully operational.".to_string(),
        };

        let reply_string = serde_json::to_string(&reply).unwrap();

        let reply_bytes = reply_string.as_bytes();
        let out_len_bytes = (reply_bytes.len() as u32).to_ne_bytes();

        io::stdout().write_all(&out_len_bytes).unwrap();
        io::stdout().write_all(reply_bytes).unwrap();
        io::stdout().flush().unwrap();
    }
}