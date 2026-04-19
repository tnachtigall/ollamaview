mod backend_action;
use backend_action::BackendAction;
use std::io::{self, ErrorKind, Read, Write};
use serde::{Deserialize, Serialize};
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
        }

        let Ok(message) = serde_json::from_str::<BrowserMessage>(&raw_json) else {
            eprintln!("FATAL: Error while deserializing json query!");
            break;
        };

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