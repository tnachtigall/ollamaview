use std::io::{self, ErrorKind, Read, Write};
use std::fs::OpenOptions;
use serde_json::json;

// Helper to write to a log file instead of stdout
fn log(msg: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open("/tmp/ollamaview.log") {
        writeln!(file, "{}", msg).ok();
    }
}

fn main() {
    log("--- RUST BACKEND STARTED ---");

    loop {
        log("Waiting for 4-byte header from LibreWolf...");
        let mut len_bytes = [0u8; 4];

        match io::stdin().read_exact(&mut len_bytes) {
            Ok(_) => {}, // Successfully read 4 bytes, continue
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                log("Disconnected: Browser closed the pipe (EOF). Shutting down.");
                break;
            },
            Err(e) => {
                log(&format!("FATAL: Unexpected read error: {}", e));
                break;
            }
        }

        let len = u32::from_ne_bytes(len_bytes) as usize;
        log(&format!("Success: Browser says incoming JSON is {} bytes long", len));

        let mut buffer = vec![0u8; len];
        if let Err(e) = io::stdin().read_exact(&mut buffer) {
            log(&format!("FATAL: Failed to read the JSON payload. Error: {}", e));
            break;
        }

        let raw_json = String::from_utf8_lossy(&buffer);
        log(&format!("Received JSON: {}", raw_json));
        
        let reply = json!({
            "text": "Success! The Rust Native Messaging bridge is fully operational."
        });

        let reply_string = reply.to_string();
        let reply_bytes = reply_string.as_bytes();
        let out_len_bytes = (reply_bytes.len() as u32).to_ne_bytes();

        log("Writing response back to Browser...");
        io::stdout().write_all(&out_len_bytes).unwrap();
        io::stdout().write_all(reply_bytes).unwrap();
        io::stdout().flush().unwrap();
        log("Response sent successfully.");
    }

    log("--- RUST BACKEND SHUT DOWN ---");
}