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
from functools import lru_cache
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TESTING_DIR = Path(__file__).resolve().parent
QUERIES_PATH = TESTING_DIR / "queries.json"
FIXTURES_PATH = TESTING_DIR / "fixture_articles.json"
RESULTS_DIR = TESTING_DIR / "results"
DEFAULT_BINARY = REPO_ROOT / "target" / "debug" / "ollamaview"

DEFAULT_SYSTEM_PROMPT = (
    "You are a precise, privacy-focused search synthesis engine. Your task is to generate a "
    "dense, objective overview that directly answers the user's query using ONLY the provided "
    "\"--- SOURCE N ---\" search excerpts.\n\n"
    "### Core Rules\n"
    "1. **Strict Grounding:** Base every statement exclusively on the provided sources. Never "
    "rely on prior training data, assume unstated facts, or extrapolate to fill gaps.\n"
    "2. **Preserve Granularity:** Retain concrete details from the sources—such as exact "
    "numbers, units, configuration flags, function/API names, dates, versions, and proper "
    "nouns—rather than summarizing them into vague generalizations.\n"
    "3. **Calibrated Claims & Contradictions:** Mirror the exact certainty of the sources. "
    "Avoid absolute language if sources describe probabilities, trade-offs, or conditions. "
    "Explicitly report conflicting viewpoints or version differences neutrally if sources disagree.\n"
    "4. **High Information Density:** Start directly with a 1–2 sentence core answer or "
    "definition. Eliminate introductory filler (e.g., \"Based on the provided sources...\"), "
    "transitional prose, and closing meta-commentary.\n"
    "5. **Formatting:** Use clean Markdown: short sections, inline code spans for technical "
    "identifiers, and bullet points starting with **Bold Key Terms:** followed by concise facts.\n"
    "6. **Strict Abstention:** If the provided sources do not contain enough information to "
    "answer the query or a part of it, briefly state what is missing and stop. Do not "
    "speculate or fall back on general knowledge."
)


# ---------------------------------------------------------------------------
# Deterministic Realistic Long-Form HTML Fixture Generator
# ---------------------------------------------------------------------------
# Generates ~6,000-character realistic technical articles where early paragraphs
# contain general background/history loaded from fixture_articles.json and deep
# paragraphs or specific source indices (Sources #1..#8) hold fact nuggets.
@lru_cache(maxsize=1)
def load_article_fixtures(fixtures_path: Path = FIXTURES_PATH) -> dict:
    """Load and validate filler paragraphs and per-query fixture articles from JSON."""
    data = json.loads(fixtures_path.read_text(encoding="utf-8"))
    filler_paragraphs = data.get("filler_paragraphs")
    if not isinstance(filler_paragraphs, list) or not filler_paragraphs or not all(
        isinstance(p, str) and p.strip() for p in filler_paragraphs
    ):
        raise ValueError(f"Expected non-empty 'filler_paragraphs' string list in {fixtures_path}")

    articles = data.get("query_fixture_articles")
    if not isinstance(articles, dict) or not articles:
        raise ValueError(f"Expected non-empty 'query_fixture_articles' object in {fixtures_path}")

    return data


def build_article_html(query_id: str, doc_idx: int, is_expanded: bool = False) -> str:
    """Build deterministic HTML for a benchmark fixture article."""
    fixtures = load_article_fixtures()
    filler_paragraphs = fixtures["filler_paragraphs"]
    spec = fixtures["query_fixture_articles"].get(query_id, {})
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

    # Add ~6,000 chars of realistic filler paragraphs BEFORE the deep fact paragraph
    # so naive prefix truncation misses the buried fact, whereas BM25 ranks the deep
    # fact paragraph to the top.
    for i in range(14):
        filler = filler_paragraphs[(doc_idx + i) % len(filler_paragraphs)]
        paragraphs.append(f"[Section {i + 1}.{doc_idx}] {filler}")

    deep_map = spec.get("deep_facts", {})
    deep_fact = deep_map.get(str(doc_idx))
    if deep_fact is not None:
        paragraphs.append(deep_fact)

    for i in range(3):
        filler = filler_paragraphs[(doc_idx + i + 3) % len(filler_paragraphs)]
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
    parser.add_argument("--helper-model", default="qwen3:1.7b", help="Secondary helper model for query expansion")
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
        for ctx in ([2048, 4096] if args.sweep == "quick" else [2048, 4096, 8192]):
            for bm25 in [False, True]:
                configs.append({
                    "sweep": "Sweep 1: BM25 & Context Size",
                    "label": f"ctx={ctx} | BM25={'ON' if bm25 else 'OFF'} (N=3)",
                    "contextSize": ctx,
                    "maxSources": 3,
                    "bm25Enabled": bm25,
                    "queryExpansion": False,
                })

    if args.sweep in ("all", "2"):
        for n_src in [1, 2, 3, 4, 5]:
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
                "label": f"Expansion={'ON (' + args.helper_model + ')' if exp else 'OFF (Baseline)'} (N=5, ctx=4096)",
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
                "includeCitations": False,
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

