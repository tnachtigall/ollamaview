# ollamaview

Local-first, zero-telemetry AI search overview for **Firefox** and Gecko-based browsers (such as **LibreWolf**, **Zen**, **Waterfox**, and **Floorp**), written in Rust and powered directly by your local **Ollama** instance.

This repo contains the native messaging backend for the frontend extension https://github.com/tnachtigall/ollamaview-ext.

When the Extension sidebar is open on your search engine (currently supported: Google, DuckDuckGo, Ecosia), `ollamaview` fetches the organic search results, ranks the relevant passages with BM25, and streams a cited Markdown summary directly from your local model. When the sidebar is closed, the extension remains completely inactive.

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
- **Firefox** or any Gecko-based derivative (**LibreWolf**, **Zen**, **Waterfox**, **Floorp**)

### 1. Build the Backend

```bash
git clone https://github.com/tnachtigall/ollamaview.git
cd ollamaview
cargo build --release
```

### 2. Register the Native Messaging Host

Create `ollamaview.host.json` with the absolute path to your compiled binary (`ollamaview` or `ollamaview.exe`):

```json
{
  "name": "ollamaview.host",
  "description": "Local AI Overview powered by Ollama",
  "path": "/absolute/path/to/ollamaview/target/release/ollamaview",
  "type": "stdio",
  "allowed_extensions": [
    "{C76103A5-77D3-4F60-AB75-4B713E7FA15E}"
  ]
}
```

Place the manifest in your OS native messaging directory:

- **Linux:** `~/.mozilla/native-messaging-hosts/ollamaview.host.json` *(LibreWolf: `~/.librewolf/native-messaging-hosts/`)*
- **macOS:** `~/Library/Application Support/Mozilla/NativeMessagingHosts/ollamaview.host.json` *(LibreWolf: `~/Library/Application Support/LibreWolf/NativeMessagingHosts/`)*
- **Windows:** Point the registry key `HKCU\Software\Mozilla\NativeMessagingHosts\ollamaview.host` to the JSON file path.

### 3. Load the Extension

> **Note:** The extension is not yet listed on the Mozilla Add-on Marketplace

1. open [`ollamaview-ext`](https://github.com/tnachtigall/ollamaview-ext)
2. follow the installation instructions in the readme of the extension repo

---

## Configuration

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

## Model Recommendations (for 8–24 GB VRAM cards, per November 2026)

> In my testing, gemma4 models performed by far the best, and unsurprisingly provided results most similar to Googles native AI Overview. 
> Feel free to experiment with other models and system prompts and share your findings!

| Tier / Role | Model | Notes |
| :--- | :--- | :--- |
| **Primary Default (12–16 GB VRAM)** | **`gemma4:12B`** | Best overall grounding, formatting, and accuracy while leaving VRAM headroom on 16 GB cards. Keep `Thinking Mode: OFF` for fast TTFT. |
| **Fast / 8–12 GB VRAM** | **`gemma4:e4b`** | Sub-second prompt prefill and fast generation on notebooks and mid range gpus. |
| **24 GB VRAM** | **TBD** | (can't test 24B models on my setup, open for suggestions, as I would like to offer a recommendation here) |
| **Expansion Helper (~1.3 GB VRAM)** | **`qwen3:1.7b`** **`gemma4:e2b`** | Fast companion model for **Query Expansion**; co-resides in VRAM alongside primary model. |

> **Tip:** When using **Query Expansion**, set `OLLAMA_MAX_LOADED_MODELS=2` and `OLLAMA_FLASH_ATTENTION=1` in your Ollama environment so both models stay loaded in VRAM.

---

## Benchmarking

A standalone evaluation harness for testing latency (TTFT) and factual recall across configurations is located in [`testing/`](./testing/README.md):

```bash
python3 testing/bench_runner.py --sweep all
```
