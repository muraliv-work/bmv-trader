"""TradingAgents for bmv-trader trial 10 (DESIGN §7f): one ticker, one date, one answer line.

Started by `bmv-trader` inside a macOS sandbox, with a cleared environment and a fresh folder per
run as HOME (so no run can read what an earlier one learned). It refuses to run if broker keys, a
redirected Anthropic endpoint or an unexpected TradingAgents setting are in reach. Everything except
the answer line goes to stderr; usage is also written call by call, so a crashed run can be priced.
"""

import json
import os
import sys

QUICK_MODEL = "claude-haiku-5-5"
DEEP_MODEL = "claude-sonnet-5-5"
SUMMARY_CHARS = 2000
RESULT_PREFIX = "BMV_RESULT "
FORBIDDEN = ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_URL", "ANTHROPIC_AUTH_TOKEN")
# the only TradingAgents settings bmv-trader passes; any other could redirect or change the run
ALLOWED_TA = ("TRADINGAGENTS_RESULTS_DIR", "TRADINGAGENTS_MEMORY_LOG_PATH", "TRADINGAGENTS_CACHE_DIR")


def check_env(when):
    bad = [
        k for k in os.environ
        if k.startswith("ALPACA") or k in FORBIDDEN
        or (k.startswith("TRADINGAGENTS_") and k not in ALLOWED_TA)
    ]
    if bad:
        sys.stderr.write(f"refusing to run {when}: {', '.join(sorted(bad))} in the environment\n")
        sys.exit(3)


def main():
    if len(sys.argv) != 3:
        sys.stderr.write("usage: run.py TICKER YYYY-MM-DD\n")
        sys.exit(2)
    ticker, date = sys.argv[1], sys.argv[2]
    check_env("before import")
    for k in ALLOWED_TA + ("BMV_USAGE_FILE",):
        if not os.environ.get(k):
            sys.stderr.write(f"refusing to run: {k} is not set\n")
            sys.exit(3)
    usage_file = os.environ["BMV_USAGE_FILE"]
    out, sys.stdout = sys.stdout, sys.stderr

    # importing tradingagents runs load_dotenv() from the working directory
    import tradingagents
    from langchain_core.callbacks import BaseCallbackHandler
    from tradingagents.default_config import DEFAULT_CONFIG
    from tradingagents.graph.trading_graph import TradingAgentsGraph

    check_env("after import")

    class Usage(BaseCallbackHandler):
        """Tokens per LLM call, with the model that answered, also appended to the usage file."""

        def __init__(self):
            self.calls = []

        def on_llm_end(self, response, **kwargs):
            for gens in response.generations:
                for g in gens:
                    m = getattr(g, "message", None)
                    usage = getattr(m, "usage_metadata", None) if m is not None else None
                    if not usage:
                        continue
                    meta = getattr(m, "response_metadata", None) or {}
                    call = {
                        "model": str(meta.get("model") or meta.get("model_name") or ""),
                        "input": int(usage.get("input_tokens", 0)),
                        "output": int(usage.get("output_tokens", 0)),
                    }
                    self.calls.append(call)
                    with open(usage_file, "a") as f:
                        f.write(json.dumps(call) + "\n")

    config = dict(DEFAULT_CONFIG)
    config.update({
        "llm_provider": "anthropic",
        "quick_think_llm": QUICK_MODEL,
        "deep_think_llm": DEEP_MODEL,
        "quick_think_provider": "anthropic",
        "deep_think_provider": "anthropic",
        "backend_url": None,
        "quick_think_backend_url": None,
        "deep_think_backend_url": None,
        "max_debate_rounds": 1,
        "max_risk_discuss_rounds": 1,
        "results_dir": os.environ["TRADINGAGENTS_RESULTS_DIR"],
        "memory_log_path": os.environ["TRADINGAGENTS_MEMORY_LOG_PATH"],
        "data_cache_dir": os.environ["TRADINGAGENTS_CACHE_DIR"],
    })
    usage = Usage()
    graph = TradingAgentsGraph(
        selected_analysts=("market", "news", "fundamentals"),
        debug=False,
        config=config,
        callbacks=[usage],
    )
    state, rating = graph.propagate(ticker, date)
    summary = str(state.get("final_trade_decision") or "")[:SUMMARY_CHARS]
    out.write(RESULT_PREFIX + json.dumps({
        "ticker": ticker,
        "date": date,
        "rating": str(rating),
        "summary": summary,
        "usage": usage.calls,
        "version": tradingagents.__version__,
    }) + "\n")
    out.flush()


if __name__ == "__main__":
    main()
