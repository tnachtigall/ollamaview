#!/usr/bin/env python3
"""
Ollamaview Benchmark & Evaluation Harness
=========================================
Drives the compiled `ollamaview` Native Messaging binary directly over stdio
to transparently measure:
  - End-to-End Time to First Token (TTFT)
  - Internal Fetch / BM25 Prep Latency vs. LLM Prefill Latency
  - Streaming Decode Speed & Total Time
  - VRAM Co-Residency via Ollama `/api/ps` (Primary + Secondary Helper Model)
  - Objective Fact-Nugget Recall (%) across 3 controlled experimental sweeps:
      Sweep 1: BM25 ON vs. OFF across Context Sizes (2048, 4096, 6144, 8192)
      Sweep 2: Source Count N (2, 3, 5, 7, 8)
      Sweep 3: Multi-Model Routing (Query Expansion OFF vs. ON with Helper Model)
"""

import argparse
import csv
import http.server
import json
import os
import re
import socketserver
import statistics
import struct
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TESTING_DIR = Path(__file__).resolve().parent
QUERIES_PATH = TESTING_DIR / "queries.json"
RESULTS_DIR = TESTING_DIR / "results"
DEFAULT_BINARY = REPO_ROOT / "target" / "debug" / "ollamaview"

DEFAULT_SYSTEM_PROMPT = (
    "You are a helpful, privacy-focused search assistant. Using the provided web search "
    "sources, write a clear, concise, and accurate overview that directly answers the "
    "user's search query. Format your response in clean Markdown (such as bullet points, "
    "bold key terms, and short sections where helpful). Synthesize key facts from the "
    "sources, stay objective, and avoid speculation. If the sources do not contain enough "
    "information to fully answer the query, briefly state what is missing and provide the "
    "best helpful answer you can."
)


# ---------------------------------------------------------------------------
# Deterministic Realistic Long-Form HTML Fixture Generator
# ---------------------------------------------------------------------------
# Generates ~4,500-character realistic technical articles where early paragraphs
# contain general background/history and deep paragraphs (65-85% down the page)
# or specific source indices (Sources #1..#8) hold the ground-truth fact nuggets.
FILLER_PARAGRAPHS = [
    (
        "In modern systems engineering, understanding the architectural history of runtime "
        "schedulers and storage engines helps contextualize how defaults evolved over time. "
        "Early implementations prioritized single-threaded simplicity over multi-core scalability, "
        "relying on cooperative yielding and global locks to coordinate shared state across workers."
    ),
    (
        "As hardware shifted toward high core counts and non-uniform memory access (NUMA) topologies, "
        "contention on centralized work queues became a primary bottleneck. Engineers introduced "
        "work-stealing deques, per-thread arenas, and asynchronous I/O readiness notification "
        "mechanisms such as epoll on Linux, kqueue on BSD/macOS, and IOCP on Windows."
    ),
    (
        "When profiling production workloads, observability tools frequently highlight tail-latency "
        "spikes caused by head-of-line blocking. Even a brief synchronous syscall or unindexed scan "
        "on a shared executor thread can stall ratusan unrelated tasks waiting in the local run queue, "
        "degrading p99 response times across the entire service mesh."
    ),
    (
        "To mitigate these risks, operators often deploy canary environments with synthetic load "
        "generators that replay production traffic patterns. Telemetry collected from eBPF probes "
        "and hardware performance counters reveals cache-miss ratios, context-switch frequencies, "
        "and page-fault latencies under sustained throughput."
    ),
    (
        "Documentation across community forums and legacy blog posts often references outdated "
        "configuration flags from earlier major releases. Always verify version-specific behavior "
        "against the canonical reference manual before applying tuning parameters in production."
    ),
    (
        "Furthermore, cross-platform compatibility layers introduce subtle differences in buffer "
        "alignment, file descriptor inheritance, and environment variable propagation when processes "
        "are launched by desktop session managers or sandboxed browser runtimes."
    ),
]

QUERY_FIXTURE_ARTICLES = {
    "q1_deep_needle_tokio": {
        # Facts buried deep at the bottom of the articles (tests BM25 vs Naive Prefix Truncation)
        "deep_facts": {
            0: (
                "Blocking Thread Pool Configuration and Limits: When an application calls "
                "spawn_blocking to offload synchronous file I/O or CPU-heavy work off the async "
                "worker threads, Tokio spawns dedicated OS threads on demand. By default, the "
                "max_blocking_threads limit is set to 512 threads per runtime instance."
            ),
            1: (
                "Idle Blocking Thread Lifecycle: Threads in the blocking pool are not kept alive "
                "indefinitely when idle. Builder::thread_keep_alive configures how long an idle "
                "blocking pool thread waits for new work before terminating; the default "
                "thread_keep_alive timeout is 10 seconds."
            ),
            2: (
                "If more than 512 blocking tasks are submitted concurrently via spawn_blocking, "
                "additional tasks wait in an unbounded FIFO queue until an active blocking thread "
                "finishes its closure or new capacity becomes available before the 10 seconds "
                "thread_keep_alive window expires."
            ),
        },
        "expansion_facts": (
            "Summary Reference Table: Tokio Runtime Builder defaults — worker_threads defaults to "
            "the number of CPU cores, max_blocking_threads defaults to 512, and thread_keep_alive "
            "defaults to 10 seconds for threads spawned via spawn_blocking."
        ),
    },
    "q2_deep_needle_sqlite": {
        "deep_facts": {
            0: (
                "Write-Ahead Logging (WAL) Checkpoint Thresholds: In SQLite WAL mode, modified "
                "pages are appended to the -wal file until a checkpoint transfers them back into "
                "the main database file. By default, PRAGMA wal_autocheckpoint is set to 1000 pages."
            ),
            1: (
                "Because the default SQLite database page_size is 4096 bytes (4 KB), the default "
                "wal_autocheckpoint threshold of 1000 pages triggers an automatic passive checkpoint "
                "whenever the write-ahead log reaches approximately 4 MB on disk."
            ),
            2: (
                "Concurrency and Lock Contention: When a writer or checkpoint encounters a locked "
                "database, SQLite immediately returns SQLITE_BUSY unless PRAGMA busy_timeout is "
                "configured to retry for a specified number of milliseconds."
            ),
        },
        "expansion_facts": (
            "SQLite WAL Tuning Quick Reference: PRAGMA wal_autocheckpoint=1000 (with 4096 byte "
            "pages) and PRAGMA busy_timeout prevent immediate SQLITE_BUSY errors under concurrent writes."
        ),
    },
    "q3_ambiguous_wayland": {
        # Initial pages only mention partial symptoms unless expanded subqueries or deeper sources are used
        "deep_facts": {
            0: (
                "Diagnosing XWayland Blur on Linux: When Firefox runs under XWayland on a HiDPI "
                "display with 125% or 150% fractional scaling, the compositor upscales the rasterized "
                "X11 buffer, resulting in blurry text. Check about:support under the Window Protocol "
                "field to see whether Firefox is using xwayland or native wayland."
            ),
            2: (
                "Enabling Native Wayland Backend: To switch Firefox from XWayland to the native "
                "Wayland protocol on older builds or custom launchers, set the environment variable "
                "MOZ_ENABLE_WAYLAND=1 in your environment or desktop entry."
            ),
            4: (
                "Native Fractional Scale Protocol: Inside about:config, toggle "
                "widget.wayland.fractional-scale.enabled to true so Firefox renders at the exact "
                "fractional pixel ratio using the wp-fractional-scale-v1 Wayland protocol."
            ),
        },
        "expansion_facts": (
            "Complete Fix for Blurry Firefox on Wayland Fractional Scaling: 1) Verify Window Protocol "
            "in about:support is wayland (set MOZ_ENABLE_WAYLAND=1 if needed). 2) In about:config, "
            "set widget.wayland.fractional-scale.enabled to true for crisp HiDPI text."
        ),
    },
    "q4_ambiguous_ollama_vram": {
        "deep_facts": {
            0: (
                "Multi-Model Co-Residency in Ollama: By default, Ollama may evict an existing model "
                "from GPU memory when a second model is requested if concurrency limits or VRAM "
                "thresholds are exceeded. Set OLLAMA_MAX_LOADED_MODELS=2 (or higher) in the server "
                "environment to allow multiple models to remain loaded simultaneously."
            ),
            2: (
                "Preventing Idle Eviction: Use OLLAMA_KEEP_ALIVE (for example OLLAMA_KEEP_ALIVE=15m "
                "or -1) or the per-request keep_alive parameter so loaded models are not unloaded "
                "after the default 5-minute inactivity timer."
            ),
            4: (
                "Reducing KV Cache VRAM Footprint: When co-hosting two models on a 16 GB GPU, enable "
                "OLLAMA_FLASH_ATTENTION=1 and set OLLAMA_KV_CACHE_TYPE=q8_0 (or q4_0) to halve KV "
                "cache memory consumption."
            ),
        },
        "expansion_facts": (
            "Ollama Multi-Model VRAM Guide: Configure OLLAMA_MAX_LOADED_MODELS=2 and OLLAMA_KEEP_ALIVE "
            "to keep both models in VRAM, and enable OLLAMA_FLASH_ATTENTION=1 with "
            "OLLAMA_KV_CACHE_TYPE=q8_0 to save GPU memory."
        ),
    },
    "q5_broad_compression": {
        # Facts distributed across sources 0..5 to test Source Count N (N=2 vs N=5 vs N=8)
        "deep_facts": {
            0: (
                "LZ4 Performance Profile: LZ4 is engineered for extreme speed, achieving compression "
                "rates over 500 MB/s per core and decompression speeds reaching multiple GB/s, making "
                "it ideal for real-time IPC and RAM disks despite a lower compression ratio."
            ),
            1: (
                "Zstandard (zstd) Balanced Design: Developed at Facebook, Zstandard bridges the gap "
                "between LZ4 and deflate, offering tunable levels from 1 to 19 plus high-compression "
                "--ultra levels (20 to 22) with fast dictionary decompression."
            ),
            3: (
                "Brotli for Static Web Assets: Brotli achieves higher compression density than "
                "default Zstandard on HTML, CSS, and JavaScript text payloads, though at higher "
                "compression levels (9-11) its encoding speed is significantly slower than LZ4 or zstd."
            ),
            4: (
                "Summary Tradeoff: Choose LZ4 when sub-millisecond latency (>500 MB/s) dominates, "
                "Zstandard (with optional --ultra levels) for general-purpose storage and archives, "
                "and Brotli for pre-compressed HTTP web assets."
            ),
        },
        "expansion_facts": (
            "Compression Benchmark Comparison: LZ4 (>500 MB/s) vs Zstandard (zstd, levels 1-19 and "
            "--ultra 20-22) vs Brotli (high web asset compression ratio)."
        ),
    },
    "q6_direct_native_messaging": {
        # Direct fact in the very first paragraph of Source #0 (baseline speed check)
        "lead_fact": (
            "Firefox WebExtensions Native Messaging Protocol Specification: Each message exchanged "
            "over stdin and stdout is serialized as JSON UTF-8 and prefixed with a 32-bit length "
            "integer in native byte order. The maximum message size sent from the native application "
            "to the browser via stdout is 1 MB, while messages sent from the browser extension to "
            "the native host via stdin can be up to 4 GB. The host manifest JSON must list the "
            "extension ID inside the allowed_extensions array."
        ),
        "deep_facts": {
            0: (
                "Ensure stdout is flushed immediately after writing the 32-bit native byte order "
                "header and JSON payload, and keep each reply under the 1 MB stdout limit."
            ),
        },
        "expansion_facts": (
            "Firefox Native Messaging limits: 32-bit native byte order length header, 1 MB max "
            "from host (stdout), 4 GB max to host (stdin), authorized via allowed_extensions."
        ),
    },
}


def build_article_html(query_id: str, doc_idx: int, is_expanded: bool = False) -> str:
    spec = QUERY_FIXTURE_ARTICLES.get(query_id, {})
    paragraphs = []

    if is_expanded and "expansion_facts" in spec:
        paragraphs.append(spec["expansion_facts"])

    if doc_idx == 0 and "lead_fact" in spec:
        paragraphs.append(spec["lead_fact"])
    else:
        paragraphs.append(
            f"Technical Reference Article #{doc_idx + 1} for {query_id}: "
            "This document provides architectural background, historical context, and operational "
            "guidelines for configuring and troubleshooting system behavior in production environments."
        )

    # Add ~3,200 chars of realistic filler paragraphs BEFORE the deep fact paragraph
    # so naive prefix truncation at small/medium budgets misses the buried fact,
    # whereas BM25 ranks the deep fact paragraph to the top!
    for i in range(8):
        filler = FILLER_PARAGRAPHS[(doc_idx + i) % len(FILLER_PARAGRAPHS)]
        paragraphs.append(f"[Section {i + 1}.{doc_idx}] {filler}")

    deep_map = spec.get("deep_facts", {})
    if doc_idx in deep_map:
        paragraphs.append(deep_map[doc_idx])

    for i in range(3):
        filler = FILLER_PARAGRAPHS[(doc_idx + i + 3) % len(FILLER_PARAGRAPHS)]
        paragraphs.append(f"[Appendix {i + 1}] {filler}")

    body_p = "\n".join(f"<p>{p}</p>" for p in paragraphs)
    return (
        f"<!DOCTYPE html><html><head><meta charset='utf-8'>"
        f"<title>Reference Doc {query_id} #{doc_idx}</title></head>"
        f"<body><article><h1>Reference Documentation ({query_id} - Source {doc_idx + 1})</h1>"
        f"{body_p}</article></body></html>"
    )


class FixtureHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def start_fixture_server(simulated_latency_ms: int = 80):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            if simulated_latency_ms > 0:
                time.sleep(simulated_latency_ms / 1000.0)

            # DuckDuckGo HTML subquery endpoint for deterministic Query Expansion testing
            if parsed.path.startswith("/ddg"):
                qs = urllib.parse.parse_qs(parsed.query)
                subq = (qs.get("q", [""])[0]).lower()
                matched_qid = "q1_deep_needle_tokio"
                if "sqlite" in subq or "wal" in subq:
                    matched_qid = "q2_deep_needle_sqlite"
                elif "wayland" in subq or "blurry" in subq or "scale" in subq or "firefox" in subq:
                    matched_qid = "q3_ambiguous_wayland"
                elif "ollama" in subq or "vram" in subq or "unload" in subq or "model" in subq:
                    matched_qid = "q4_ambiguous_ollama_vram"
                elif "zstd" in subq or "lz4" in subq or "brotli" in subq or "compress" in subq:
                    matched_qid = "q5_broad_compression"
                elif "native" in subq or "stdin" in subq:
                    matched_qid = "q6_direct_native_messaging"

                host = f"http://127.0.0.1:{self.server.server_port}"
                u1 = urllib.parse.quote(f"{host}/expanded/{matched_qid}/0", safe="")
                u2 = urllib.parse.quote(f"{host}/expanded/{matched_qid}/1", safe="")
                ddg_html = (
                    f"<html><body>"
                    f"<a class='result__a' href='//duckduckgo.com/l/?uddg={u1}&rut=1'>Result 1</a>"
                    f"<a class='result__a' href='//duckduckgo.com/l/?uddg={u2}&rut=2'>Result 2</a>"
                    f"</body></html>"
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.end_headers()
                self.wfile.write(ddg_html.encode("utf-8"))
                return

            parts = [p for p in parsed.path.split("/") if p]
            # /article/<query_id>/<doc_idx> or /expanded/<query_id>/<doc_idx>
            if len(parts) >= 3 and parts[0] in ("article", "expanded"):
                is_exp = parts[0] == "expanded"
                qid = parts[1]
                try:
                    d_idx = int(parts[2])
                except ValueError:
                    d_idx = 0
                html = build_article_html(qid, d_idx, is_expanded=is_exp)
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.end_headers()
                self.wfile.write(html.encode("utf-8"))
                return

            self.send_response(404)
            self.end_headers()

    server = FixtureHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


# ---------------------------------------------------------------------------
# Ollama API Helpers (Warm-up & `/api/ps` VRAM Residency Inspection)
# ---------------------------------------------------------------------------
def ollama_get_json(endpoint: str, path: str, timeout: float = 5.0):
    url = f"{endpoint.rstrip('/')}{path}"
    req = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def ollama_warmup_model(endpoint: str, model: str, num_ctx: int = 4096):
    url = f"{endpoint.rstrip('/')}/api/generate"
    payload = json.dumps({
        "model": model,
        "prompt": "Hi",
        "stream": False,
        "think": False,
        "keep_alive": "15m",
        "options": {"num_ctx": num_ctx, "num_predict": 1},
    }).encode("utf-8")
    req = urllib.request.Request(
        url, data=payload, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=60.0) as resp:
        resp.read()


def get_ollama_vram_snapshot(endpoint: str):
    try:
        data = ollama_get_json(endpoint, "/api/ps")
        models = []
        for m in data.get("models", []):
            name = m.get("name", "unknown")
            size_bytes = m.get("size", 0)
            vram_bytes = m.get("size_vram", 0)
            vram_gb = vram_bytes / (1024 ** 3)
            pct_gpu = (vram_bytes / size_bytes * 100.0) if size_bytes else 0.0
            models.append(f"{name} ({vram_gb:.2f} GB VRAM, {pct_gpu:.0f}% GPU)")
        return models
    except Exception as e:
        return [f"Error querying /api/ps: {e}"]


# ---------------------------------------------------------------------------
# Native Messaging Binary Driver
# ---------------------------------------------------------------------------
def run_single_query(
    binary_path: Path,
    payload: dict,
    ddg_override_url: str = None,
    timeout_sec: float = 90.0,
):
    env = os.environ.copy()
    if ddg_override_url:
        env["OLLAMAVIEW_DDG_URL"] = ddg_override_url

    log_path = Path(env.get("HOME", "/tmp")) / "ollama_host.log"

    proc = subprocess.Popen(
        [str(binary_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

    raw_bytes = json.dumps(payload).encode("utf-8")
    frame = struct.pack("@I", len(raw_bytes)) + raw_bytes

    t0 = time.perf_counter()
    proc.stdin.write(frame)
    proc.stdin.flush()

    ttft_ms = None
    final_message = ""
    final_status = "Unknown"
    frame_count = 0

    try:
        while True:
            hdr = proc.stdout.read(4)
            if not hdr or len(hdr) < 4:
                break
            msg_len = struct.unpack("@I", hdr)[0]
            body_bytes = proc.stdout.read(msg_len)
            now = time.perf_counter()
            elapsed_ms = (now - t0) * 1000.0

            msg = json.loads(body_bytes.decode("utf-8"))
            frame_count += 1
            status = msg.get("status", "")

            if ttft_ms is None and status in ("Streaming", "Success", "Error"):
                ttft_ms = elapsed_ms

            if status in ("Success", "Error"):
                final_status = status
                final_message = msg.get("message", "")
                total_ms = elapsed_ms
                break
        else:
            total_ms = (time.perf_counter() - t0) * 1000.0
    finally:
        try:
            proc.stdin.close()
            proc.wait(timeout=3.0)
        except Exception:
            proc.kill()

    # Parse internal fetch_ms and context_bytes from ~/ollama_host.log
    fetch_ms = None
    context_bytes = 0
    sources_used = 0
    if log_path.exists():
        try:
            log_text = log_path.read_text(encoding="utf-8", errors="ignore")
            matches = re.findall(
                r"Prepared context from (\d+) sources \((\d+) bytes, num_ctx=\d+, bm25=\w+, fetch_ms=(\d+)\)",
                log_text,
            )
            if matches:
                s_cnt, c_bytes, f_ms = matches[-1]
                sources_used = int(s_cnt)
                context_bytes = int(c_bytes)
                fetch_ms = float(f_ms)
        except Exception:
            pass

    prefill_ms = (ttft_ms - fetch_ms) if (ttft_ms is not None and fetch_ms is not None) else None

    return {
        "status": final_status,
        "ttft_ms": round(ttft_ms, 1) if ttft_ms is not None else None,
        "fetch_ms": round(fetch_ms, 1) if fetch_ms is not None else None,
        "prefill_ms": round(max(0.0, prefill_ms), 1) if prefill_ms is not None else None,
        "total_ms": round(total_ms, 1),
        "sources_used": sources_used,
        "context_bytes": context_bytes,
        "output_chars": len(final_message),
        "message": final_message,
    }


def score_nuggets(text: str, nuggets: list) -> tuple:
    if not nuggets:
        return 100.0, []
    lower_text = text.lower()
    hits = []
    misses = []
    for nug in nuggets:
        if nug.lower() in lower_text:
            hits.append(nug)
        else:
            misses.append(nug)
    recall_pct = (len(hits) / len(nuggets)) * 100.0
    return round(recall_pct, 1), misses


# ---------------------------------------------------------------------------
# Main Benchmark Suite Execution
# ---------------------------------------------------------------------------
def main():
    parser = argparse.ArgumentParser(description="Ollamaview Benchmark & Evaluation Harness")
    parser.add_argument("--endpoint", default="http://127.0.0.1:11434", help="Ollama endpoint URL")
    parser.add_argument("--main-model", default="gemma4:12B", help="Primary Ollama model")
    parser.add_argument("--helper-model", default="qwen:1.7B", help="Secondary helper model for query expansion")
    parser.add_argument("--binary", default=str(DEFAULT_BINARY), help="Path to compiled ollamaview binary")
    parser.add_argument("--sweep", choices=["all", "1", "2", "3", "quick"], default="all", help="Which sweep(s) to run")
    parser.add_argument("--sim-latency-ms", type=int, default=80, help="Simulated per-URL network latency (ms) on local fixture server")
    parser.add_argument("--live-ddg", action="store_true", help="Use live DuckDuckGo HTML in Sweep 3 instead of fixture DDG")
    args = parser.parse_args()

    binary_path = Path(args.binary)
    if not binary_path.exists():
        print(f"[ERROR] Binary not found at {binary_path}. Run `cargo build` first.", file=sys.stderr)
        sys.exit(1)

    # Check Ollama connectivity and installed models
    try:
        tags = ollama_get_json(args.endpoint, "/api/tags")
        installed = [m.get("name", "") for m in tags.get("models", [])]
    except Exception as e:
        print(f"[ERROR] Could not connect to Ollama at {args.endpoint}: {e}", file=sys.stderr)
        sys.exit(1)

    print(f"=== Ollamaview Benchmark Harness ===")
    print(f"Endpoint     : {args.endpoint}")
    print(f"Installed    : {', '.join(installed) if installed else '(none)'}")
    print(f"Primary Model: {args.main_model}")
    print(f"Helper Model : {args.helper_model}")

    queries_data = json.loads(QUERIES_PATH.read_text(encoding="utf-8"))["queries"]
    if args.sweep == "quick":
        queries_data = queries_data[:2]

    server = start_fixture_server(simulated_latency_ms=args.sim_latency_ms)
    port = server.server_port
    ddg_override = None if args.live_ddg else f"http://127.0.0.1:{port}/ddg"
    print(f"Fixture HTTP : http://127.0.0.1:{port} (simulated latency: {args.sim_latency_ms}ms)")

    # Build configurations for requested sweeps
    configs = []
    if args.sweep in ("all", "1", "quick"):
        for ctx in ([2048, 4096] if args.sweep == "quick" else [2048, 4096, 6144, 8192]):
            for bm25 in [False, True]:
                configs.append({
                    "sweep": "Sweep 1: BM25 & Context Size",
                    "label": f"ctx={ctx} | BM25={'ON' if bm25 else 'OFF'}",
                    "contextSize": ctx,
                    "maxSources": 5,
                    "bm25Enabled": bm25,
                    "queryExpansion": False,
                })

    if args.sweep in ("all", "2"):
        for n_src in [2, 3, 5, 7, 8]:
            configs.append({
                "sweep": "Sweep 2: Source Count (N)",
                "label": f"N={n_src} sources (ctx=4096, BM25=ON)",
                "contextSize": 4096,
                "maxSources": n_src,
                "bm25Enabled": True,
                "queryExpansion": False,
            })

    if args.sweep in ("all", "3", "quick"):
        for exp in [False, True]:
            configs.append({
                "sweep": "Sweep 3: Multi-Model Query Expansion",
                "label": f"Expansion={'ON (' + args.helper_model + ')' if exp else 'OFF (Baseline)'}",
                "contextSize": 4096,
                "maxSources": 5,
                "bm25Enabled": True,
                "queryExpansion": exp,
            })

    print(f"\nWarming up primary model ({args.main_model})...")
    try:
        ollama_warmup_model(args.endpoint, args.main_model, num_ctx=4096)
    except Exception as e:
        print(f"  [WARN] Warm-up failed for {args.main_model}: {e}")

    if any(c["queryExpansion"] for c in configs):
        print(f"Warming up helper model ({args.helper_model})...")
        try:
            ollama_warmup_model(args.endpoint, args.helper_model, num_ctx=1024)
        except Exception as e:
            print(f"  [WARN] Warm-up failed for {args.helper_model}: {e}")

    vram_before = get_ollama_vram_snapshot(args.endpoint)
    print(f"VRAM State (/api/ps): {'; '.join(vram_before)}\n")

    all_rows = []
    summary_rows = []
    last_ctx = 4096

    for cfg in configs:
        if cfg["contextSize"] != last_ctx:
            try:
                ollama_warmup_model(args.endpoint, args.main_model, num_ctx=cfg["contextSize"])
            except Exception:
                pass
            last_ctx = cfg["contextSize"]

        print(f"--- Running [{cfg['sweep']}] {cfg['label']} ---")
        ttfts, prefills, fetches, totals, recalls = [], [], [], [], []

        for q in queries_data:
            qid = q["id"]
            n_cand = cfg["maxSources"] + 3
            urls = [f"http://127.0.0.1:{port}/article/{qid}/{i}" for i in range(n_cand)]

            payload = {
                "query": q["query"],
                "urls": urls,
                "ollamaInstance": args.endpoint,
                "mainModel": args.main_model,
                "helperModel": args.helper_model,
                "thinkingMode": False,
                "maxSources": cfg["maxSources"],
                "contextSize": cfg["contextSize"],
                "bm25Enabled": cfg["bm25Enabled"],
                "queryExpansion": cfg["queryExpansion"],
                "responseLanguage": "English",
                "systemPrompt": DEFAULT_SYSTEM_PROMPT,
            }

            metrics = run_single_query(binary_path, payload, ddg_override_url=ddg_override)
            recall_pct, missed = score_nuggets(metrics["message"], q["nuggets"])

            if metrics["ttft_ms"] is not None:
                ttfts.append(metrics["ttft_ms"])
            if metrics["prefill_ms"] is not None:
                prefills.append(metrics["prefill_ms"])
            if metrics["fetch_ms"] is not None:
                fetches.append(metrics["fetch_ms"])
            totals.append(metrics["total_ms"])
            recalls.append(recall_pct)

            print(
                f"  [{qid}] TTFT={metrics['ttft_ms']}ms "
                f"(fetch={metrics['fetch_ms']}ms, prefill={metrics['prefill_ms']}ms) | "
                f"Total={metrics['total_ms']}ms | Recall={recall_pct}% "
                f"{'(missed: ' + ', '.join(missed) + ')' if missed else '(ALL HIT)'}"
            )

            all_rows.append({
                "sweep": cfg["sweep"],
                "config": cfg["label"],
                "query_id": qid,
                "ttft_ms": metrics["ttft_ms"],
                "fetch_ms": metrics["fetch_ms"],
                "prefill_ms": metrics["prefill_ms"],
                "total_ms": metrics["total_ms"],
                "sources_used": metrics["sources_used"],
                "context_bytes": metrics["context_bytes"],
                "recall_pct": recall_pct,
                "missed_nuggets": "; ".join(missed),
            })

        avg_ttft = round(statistics.mean(ttfts), 1) if ttfts else 0.0
        avg_fetch = round(statistics.mean(fetches), 1) if fetches else 0.0
        avg_prefill = round(statistics.mean(prefills), 1) if prefills else 0.0
        avg_total = round(statistics.mean(totals), 1) if totals else 0.0
        avg_recall = round(statistics.mean(recalls), 1) if recalls else 0.0
        efficiency = round(avg_recall / (avg_ttft / 1000.0), 1) if avg_ttft > 0 else 0.0

        summary_rows.append({
            "sweep": cfg["sweep"],
            "config": cfg["label"],
            "avg_ttft_ms": avg_ttft,
            "avg_fetch_ms": avg_fetch,
            "avg_prefill_ms": avg_prefill,
            "avg_total_ms": avg_total,
            "avg_recall_pct": avg_recall,
            "efficiency": efficiency,
        })
        print(
            f"  => AVG: TTFT={avg_ttft}ms | Prefill={avg_prefill}ms | "
            f"Total={avg_total}ms | Recall={avg_recall}% | Efficiency={efficiency} recall%/s\n"
        )

    vram_after = get_ollama_vram_snapshot(args.endpoint)
    server.shutdown()

    # Write results to testing/results/
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    csv_path = RESULTS_DIR / f"bench_{ts}.csv"
    md_path = RESULTS_DIR / f"bench_{ts}.md"
    latest_md = RESULTS_DIR / "latest_report.md"

    with csv_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=list(all_rows[0].keys()))
        writer.writeheader()
        writer.writerows(all_rows)

    md_lines = [
        "# Ollamaview Benchmark Report",
        f"- **Date:** {datetime.now().isoformat(timespec='seconds')}",
        f"- **Primary Model:** `{args.main_model}`",
        f"- **Helper Model:** `{args.helper_model}`",
        f"- **VRAM Before (`/api/ps`):** `{'; '.join(vram_before)}`",
        f"- **VRAM After (`/api/ps`):** `{'; '.join(vram_after)}`",
        "",
        "## Summary by Configuration",
        "",
        "| Sweep | Configuration | Avg TTFT (ms) | Avg Fetch/Prep (ms) | Avg LLM Prefill (ms) | Avg Total (ms) | Fact Recall (%) | Efficiency (Recall%/s) |",
        "| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for r in summary_rows:
        md_lines.append(
            f"| {r['sweep']} | `{r['config']}` | **{r['avg_ttft_ms']}** | {r['avg_fetch_ms']} | "
            f"{r['avg_prefill_ms']} | {r['avg_total_ms']} | **{r['avg_recall_pct']}%** | {r['efficiency']} |"
        )

    md_text = "\n".join(md_lines) + "\n"
    md_path.write_text(md_text, encoding="utf-8")
    latest_md.write_text(md_text, encoding="utf-8")

    print("=== Final Summary Table ===")
    print(md_text)
    print(f"Saved detailed CSV to: {csv_path}")
    print(f"Saved Markdown report: {latest_md}")


if __name__ == "__main__":
    main()
