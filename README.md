# ollamaview

A local-first, zero-telemetry AI search overview for **Firefox** and **LibreWolf**, written in Rust and powered strictly by your local **Ollama** instance.

When the sidebar is open on **Google**, **DuckDuckGo**, or **Ecosia**, `ollamaview` fetches the organic search results, ranks the relevant passages with BM25, and streams a cited Markdown summary directly from your local model. When the sidebar is closed, the extension remains completely inactive.

---

## Features

- **100% Local & Private:** No cloud APIs, external proxies, or background activity when the sidebar is closed.
- **Organic Sources Only:** Ignores ads and search-engine AI blocks; extracts only external result pages.
- **Fast Local RAG:** Concurrent page fetching, BM25 passage ranking, and real-time token streaming over native messaging.
- **Clickable Citations:** Inline source badges (`[1]`, `[2]`) and a reference link list to verify claims.
- **Optional Multi-Model Routing:** Can use a small secondary model in parallel to expand queries across technical facets.

---

## Setup

### Prerequisites
- [Rust / Cargo](https://rustup.rs/)
- [Ollama](https://ollama.com/) running locally
- **Firefox** or **LibreWolf**

### 1. Build the Backend

```bash
git clone https://github.com/till/ollamaview.git
cd ollamaview
cargo build --release
```

### 2. Register the Native Messaging Host

Create `com.till.ollamaview.json` with the absolute path to your compiled binary (`ollamaview` or `ollamaview.exe`):

```json
{
  "name": "com.till.ollamaview",
  "description": "Local AI Overview powered by Ollama",
  "path": "/absolute/path/to/ollamaview/target/release/ollamaview",
  "type": "stdio",
  "allowed_extensions": [
    "{C76103A5-77D3-4F60-AB75-4B713E7FA15E}"
  ]
}
```

Place the manifest in your OS native messaging directory:

- **Linux:** `~/.mozilla/native-messaging-hosts/com.till.ollamaview.json` *(LibreWolf: `~/.librewolf/native-messaging-hosts/`)*
- **macOS:** `~/Library/Application Support/Mozilla/NativeMessagingHosts/com.till.ollamaview.json` *(LibreWolf: `~/Library/Application Support/LibreWolf/NativeMessagingHosts/`)*
- **Windows:** Point the registry key `HKCU\Software\Mozilla\NativeMessagingHosts\com.till.ollamaview` to the JSON file path.

### 3. Load the Extension

1. Clone [`ollamaview-ext`](https://github.com/till/ollamaview-ext).
2. Open `about:debugging#/runtime/this-firefox` and click **Load Temporary Add-on...**
3. Select `manifest.json` from `ollamaview-ext`.
4. Open the sidebar (`Ctrl+B`), click **⚙ Config**, click **Fetch**, and select a model.

---

## Configuration

Clicking **Apply Recommended Config** resets all settings (except your endpoint and model selection) to defaults.

### Basic Settings
- **Ollama Endpoint** (`http://127.0.0.1:11434`): Local or LAN Ollama URL.
- **Model**: Primary model used to generate the overview.
- **UI Language / AI Response Language**: Separate controls for the sidebar interface and the generated answer language (`Auto` matches the search query).
- **Thinking Mode** (`OFF`): Enables/disables reasoning chains (`think`). Keep **OFF** for fast first-token latency.
- **Include Citations** (`ON`): Adds inline `[1]`, `[2]` source links and a reference list.

### Advanced Settings
- **Sources** (`5`, range `2–8`): Number of search results combined into the context budget.
- **Context Size** (`4096`, range `2048–8192`): Token context window (`2048` = fastest TTFT, `4096` = balanced, `8192` = deep).
- **Query Expansion** (`OFF`): Uses a lightweight helper model in parallel to generate orthogonal subqueries and discover extra sources.
- **System Prompt**: Customizable synthesis instructions (strictly grounded in sources by default).

---

## Recommended Models (8–24 GB VRAM)

| Tier / Role | Model | Notes |
| :--- | :--- | :--- |
| **Primary Default (12–16 GB VRAM)** | **`gemma4:12B`** | Best overall grounding, formatting, and accuracy while leaving VRAM headroom on 16 GB cards. Keep `Thinking Mode: OFF` for fast TTFT. |
| **Fast / 8–12 GB VRAM** | **`qwen3:8b`** / **`llama3.1:8b`** | Sub-second prompt prefill and fast generation on midrange GPUs. |
| **24 GB VRAM** | **`qwen3:14b`** / **`mistral-small:24b`** | Strongest synthesis on dense, multi-source technical queries. |
| **Expansion Helper (~1.3 GB VRAM)** | **`qwen3:1.7b`** | Fast companion model for **Query Expansion**; co-resides in VRAM alongside `gemma4:12B`. |

> **Tip:** When using **Query Expansion**, set `OLLAMA_MAX_LOADED_MODELS=2` and `OLLAMA_FLASH_ATTENTION=1` in your Ollama environment so both models stay loaded in VRAM.

---

## Benchmarking

A standalone evaluation harness for testing latency (TTFT) and factual recall across configurations is located in [`testing/`](./testing/README.md):

```bash
python3 testing/bench_runner.py --sweep all
```
