# ollamaview

A local-first, zero-telemetry AI search overview engine for **Firefox** and **LibreWolf**, written in Rust and powered exclusively by your local **Ollama** instance.

`ollamaview` runs as a native messaging host (`stdio`) paired with a lightweight WebExtension sidebar ([ollamaview-ext](https://github.com/till/ollamaview-ext)). When the sidebar is open on a supported search engine (**Google**, **DuckDuckGo**, **Ecosia**), it scrapes the organic result URLs, fetches and cleans the HTML concurrently in Rust, ranks the most relevant paragraphs using in-memory **BM25**, and streams a grounded, cited Markdown synthesis directly from your local GPU.

---

## Core Design Principles

- **Strictly Local & Passive:** No cloud APIs, no external proxies, and no background scraping when the sidebar is closed (`browser.sidebarAction.isOpen` gated).
- **Organic Sources Only:** Extracts only organic external result URLs from the SERP—Google's own AI Overview blocks, ads, and internal search links are ignored.
- **Fast Concurrent Quorum Scraping:** Over-fetches candidate URLs concurrently via `tokio` (`FuturesUnordered`), filters out JS shells and cookie-wall stubs (`< 80` chars of body text) before counting toward the source quorum, and caps per-page downloads at `256 KB`.
- **Lead-Preserving BM25 Paragraph Ranking:** Retains each article's lead paragraph for context and scores all body paragraphs (`<p>`, `<li>`, `<pre>`, `<table>`, `<h2>`–`<h4>`) with BM25 ($k_1 = 1.5, b = 0.75$), including Unicode word and CJK character bi-gram tokenization.
- **Hallucination-Proof Inline Citations:** Models cite numbered `--- SOURCE N ---` blocks (`[1]`, `[2]`); the Rust host validates source indices in real time and rewrites them into clickable pill badges with a reference list at the bottom of the overview.
- **Optional Multi-Model Orthogonal Facet Expansion:** Can run a lightweight secondary model (e.g. `qwen3:1.7b`) in parallel with initial HTTP downloads to expand the query along orthogonal technical facets, enriching BM25 keywords and pulling in supplementary sources via DuckDuckGo HTML.

---

## Architecture

```text
Firefox / LibreWolf SERP (Google, DuckDuckGo, Ecosia)
  │  1. Scrape top N+5 organic URLs (only while sidebar is open)
  ▼
WebExtension (background.js ──► Native Messaging stdio ──► ollamaview Rust binary)
  │  2. Concurrent HTTP fetch (3.4s deadline) + stub/cookie-wall filtering
  │  3. Optional parallel Stage-1 Helper Model (orthogonal facet expansion + DDG fetch)
  │  4. DOM noise stripping (nav/footer/aside/script) + Lead-Preserving BM25 ranking
  ▼
Local Ollama Instance (POST /api/generate, stream: true)
  │  5. Stream NDJSON tokens (~40ms frame coalescing) + real-time [1] citation linkification
  ▼
Sidebar UI (sidebar.js)
     6. Safe DOM Markdown rendering, Firefox theme sync, clickable citation pills
```

---

## Installation & Setup (Linux / Arch)

### Prerequisites

- `rust` / `cargo`
- `ollama` running locally (`systemctl --user start ollama` or `ollama serve`)
- **Firefox** or **LibreWolf**

### 1. Build the Rust Native Host

```bash
git clone https://github.com/till/ollamaview.git
cd ollamaview
cargo build --release
```

*(For development builds, `cargo build` places the binary at `target/debug/ollamaview`.)*

### 2. Register the Native Messaging Host Manifest

Create the native messaging host manifest pointing to your compiled binary. Note that the filename **must** match the `"name"` field (`com.till.ollamaview.json`):

- **Firefox:** `~/.mozilla/native-messaging-hosts/com.till.ollamaview.json`
- **LibreWolf:** `~/.librewolf/native-messaging-hosts/com.till.ollamaview.json`

```json
{
  "name": "com.till.ollamaview",
  "description": "AI Overview for Firefox / LibreWolf powered by local Ollama instance",
  "path": "/absolute/path/to/ollamaview/target/release/ollamaview",
  "type": "stdio",
  "allowed_extensions": [
    "{C76103A5-77D3-4F60-AB75-4B713E7FA15E}"
  ]
}
```

### 3. Load the Browser Extension

1. Clone the companion extension repository (`ollamaview-ext`).
2. Open `about:debugging#/runtime/this-firefox` in Firefox or LibreWolf.
3. Click **Load Temporary Add-on...** and select `manifest.json` inside `ollamaview-ext`.
4. Toggle the sidebar open (`Ctrl+B` or toolbar icon), click **⚙ Config**, click **Fetch** next to your Ollama endpoint, and select your model.

---

## Configuration Options

Settings in the sidebar are split into **Basic** and **Advanced** sections. Clicking **Apply Recommended Config** resets all parameters below (except your Ollama Endpoint and selected models) to their benchmarked defaults.

### Basic Settings

| Option | Default | Description |
| :--- | :--- | :--- |
| **Ollama Endpoint** | `http://127.0.0.1:11434` | URL of your local or LAN Ollama server. Click **Fetch** to query `/api/tags`. |
| **Model** | *(First installed)* | Primary LLM used to synthesize the search overview. |
| **UI Language** | *(Browser locale)* | Interface language (`en`, `de`, `fr`, `es`, `it`, `pt`, `nl`, `pl`, `ru`, `zh`, `ja`, `ko`, `hi`). |
| **AI Response Language** | `Auto` | Output language for the overview (`Auto` matches the search query language, or force a specific language). |
| **Thinking Mode** | `OFF` (`false`) | Passes `"think": true/false` to Ollama. Keep **OFF** by default so reasoning models (`gemma4`, `qwen3`) skip long internal CoT chains and stream answers immediately. |
| **Include Citations** | `ON` (`true`) | Appends clickable inline source badges (`[1]`, `[2]`) at the end of key bullet points and paragraphs, plus a reference link list at the bottom. |

### Advanced Settings

| Option | Default | Range | Trade-off & Behavior |
| :--- | :--- | :--- | :--- |
| **Sources ($N$)** | `5` *(Balanced)* | `2` – `8` | Number of scraped web pages incorporated into context. Total context characters are capped by `Context Size` and divided across $N$ sources, so `N=5` has the same LLM prefill time as `N=3` while capturing broader cross-site facts. |
| **Context Size (`num_ctx`)** | `4096` *(Balanced)* | `2048` – `8192` | Ollama KV context window (`2048` = sub-600ms TTFT; `4096` = ~1.1s prefill sweet spot; `8192` = deep long-form extraction at ~2x prefill latency). |
| **Query Expansion** | `OFF` (`false`) | `ON` / `OFF` | Runs a secondary helper model in parallel to generate 2 orthogonal technical subqueries, enriching BM25 terms and reserving 1 source slot for an expanded DuckDuckGo result. Adds `~240 ms` to TTFT and boosts multi-hop recall. |
| **Expansion Helper Model** | *(Small model)* | Any model | Lightweight model (1B–3B recommended) used when **Query Expansion** is enabled. |
| **System Prompt** | *6-Rule Grounded* | Custom text | Enforces strict grounding on `--- SOURCE N ---` blocks, technical granularity (exact flags, APIs, numbers), neutral reporting of source contradictions, and strict abstention instead of training-data fallback. |

---

## Recommended Models (8 GB – 24 GB VRAM)

For RAG search synthesis, instruction adherence, strict source grounding, and low Time-to-First-Token (TTFT) matter much more than open-ended creative writing.

| Role / VRAM Tier | Recommended Model | Why It Excels |
| :--- | :--- | :--- |
| **Primary Recommended (12–16 GB VRAM)** | **`gemma4:12B`** | **Best overall balance.** Exceptional grounding fidelity, formatting discipline, and multilingual synthesis while leaving plenty of VRAM headroom on 16 GB cards for desktop compositors or a helper model. *(Keep `Thinking Mode: OFF` for ~1.1s TTFT at `ctx=4096`.)* |
| **Fast / Midrange (8–12 GB VRAM)** | **`qwen3:8b`** / **`llama3.1:8b`** | Sub-second prompt prefill and high token/s throughput on 8 GB – 12 GB cards with strong technical fact extraction. |
| **High-VRAM Workstation (24 GB VRAM)** | **`qwen3:14b`** / **`mistral-small:24b`** | Maximum synthesis accuracy on dense, multi-page documentation and conflicting sources when paired with `ctx=6144` or `8192`. |
| **Stage-1 Expansion Helper (~1.3 GB VRAM)** | **`qwen3:1.7b`** | Ideal companion for `gemma4:12B` on a 16 GB GPU. Occupies only `~1.26 GB` VRAM (`100% GPU` co-resident) and generates orthogonal facet subqueries in `~250 ms`. |

### Ollama Environment Tuning (16 GB VRAM Co-Residency)

If you use **Query Expansion** (`gemma4:12B` + `qwen3:1.7b`), add these environment variables to your `ollama.service` override (`systemctl --user edit ollama.service`) so both models stay pinned in VRAM without unloading between searches:

```ini
[Service]
Environment="OLLAMA_MAX_LOADED_MODELS=2"
Environment="OLLAMA_KEEP_ALIVE=15m"
Environment="OLLAMA_FLASH_ATTENTION=1"
Environment="OLLAMA_KV_CACHE_TYPE=q8_0"
```

---

## Benchmarking & Evaluation Harness

The repository includes an automated, reproducible benchmark suite under [`testing/`](./testing/README.md) that drives the compiled `ollamaview` binary directly over native messaging `stdio` against a deterministic local fixture server and your local Ollama instance:

```bash
# Quick smoke test
python3 testing/bench_runner.py --sweep quick

# Full evaluation across BM25, Context Size, Source Count N, and Multi-Model Query Expansion
python3 testing/bench_runner.py --sweep all --main-model gemma4:12B --helper-model qwen3:1.7b
```

Logs for runtime debugging and fetch timing telemetry (`fetch_ms`, `num_ctx`, `bm25`, source count) are written to `~/ollama_host.log`.
