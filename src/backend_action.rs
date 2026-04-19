use serde::{Serialize, Deserialize};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BackendAction {
    // Serde will now merge these fields into the same object as the "type" field
    RustReply {
        status: String,
        received_query: String,
        message: String,
    },
    PerformSearch {
        queries: Vec<String>,
    },
}