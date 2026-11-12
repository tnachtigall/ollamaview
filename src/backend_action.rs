use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BackendAction {
    // Serde will now merge these fields into the same object as the "type" field
    RustReply {
        status: String,
        received_query: String,
        message: String,
    },
    ModelsList {
        status: String,
        models: Vec<String>,
        message: String,
    },
    #[allow(dead_code)]
    PerformSearch {
        queries: Vec<String>,
    },
}