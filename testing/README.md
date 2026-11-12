# Ollamaview Benchmark & Evaluation Harness

Directly drives the compiled `target/debug/ollamaview` Native Messaging binary over `stdin`/`stdout` with a deterministic local HTTP fixture server to evaluate **Time to First Token (TTFT)**, **LLM Prefill Latency**, **VRAM Co-Residency**, and **Fact-Nugget Recall (%)**.

## Quick Start

1. Make sure your models are pulled and `ollama serve` is running:
   ```bash
   ollama pull gemma4:12B
   ollama pull qwen:1.7B
   ```
   *(Tip: set `OLLAMA_MAX_LOADED_MODELS=2` on your Ollama server so both models remain co-resident in VRAM without disk swapping).*

2. Run the full 3-sweep benchmark:
   ```bash
   python3 testing/bench_runner.py --main-model gemma4:12B --helper-model qwen:1.7B
   ```

3. Run a quick smoke test or an individual sweep:
   ```bash
   # Quick smoke test (2 queries)
   python3 testing/bench_runner.py --sweep quick

   # Sweep 1 only: BM25 ON vs OFF across Context Sizes (2048, 4096, 6144, 8192)
   python3 testing/bench_runner.py --sweep 1

   # Sweep 2 only: Source Count N (2, 3, 5, 7, 8)
   python3 testing/bench_runner.py --sweep 2

   # Sweep 3 only: Multi-Model Query Expansion (OFF vs ON)
   python3 testing/bench_runner.py --sweep 3
   ```

## Output

Reports are written to `testing/results/latest_report.md` and `testing/results/bench_<timestamp>.csv`.
