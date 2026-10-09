"""Probe worldwide premium-only search filters using one authenticated session.
Reports only aggregate counts and sampled flags, never auth material.
"""
import json
import os
import subprocess
import sys
import time
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from urllib.error import HTTPError
from playwright.sync_api import sync_playwright

ENDPOINT = "https://www.geocaching.com/api/proxy/web/search/v2"
WORLD = {"box": "90,-180,-90,180", "rad": "16000", "take": "40",
         "skip": "0", "app": "geosweep"}
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/126.0 Safari/537.36"


def request(token, extra):
    p = dict(WORLD)
    p.update(extra)
    req = Request(f"{ENDPOINT}?{urlencode(p)}", headers={
        "Authorization": f"Bearer {token}", "Accept": "application/json", "User-Agent": UA,
    })
    try:
        with urlopen(req, timeout=45) as resp:
            data = json.load(resp)
    except HTTPError as exc:
        if exc.code == 429:
            print("Received 429; stopping probes to avoid throttling.", flush=True)
            sys.exit(2)
        return {"status": exc.code}
    rows = data.get("results", [])
    flags = [r.get("premiumOnly") for r in rows]
    return {"total": data.get("total"), "size": len(rows),
            "pm_true": sum(x is True for x in flags),
            "pm_false": sum(x is False for x in flags),
            "pm_null": sum(x is None for x in flags),
            "keys": sorted(rows[0])[:32] if rows else []}


def main():
    result = subprocess.run(
        [sys.executable, ".github/scripts/gc-login.py",
         "--state", "/tmp/gc-premium-count-state.json",
         "--screenshot", "/tmp/gc-premium-auth-fail.png"],
        capture_output=True, text=True, timeout=140,
    )
    if result.returncode:
        print(result.stderr[-700:], file=sys.stderr)
        raise RuntimeError("one-time browser auth failed")
    token = json.loads(result.stdout)["access_token"]

    # Examine the authenticated website itself for filter names.
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=True)
        context = browser.new_context(storage_state="/tmp/gc-premium-count-state.json")
        page = context.new_page()
        try:
            page.goto("https://www.geocaching.com/play/search",
                      wait_until="domcontentloaded", timeout=35000)
            print("Search page after login:", page.url.split("?")[0], flush=True)
            srcs = page.locator("script[src]").evaluate_all(
                "(scripts) => scripts.map(s => s.src)")
            print("Site JS resources:", len(srcs))
            names = [x.rsplit("/", 1)[-1][:90] for x in srcs]
            print("JS names:", names[:18], flush=True)
        except Exception as exc:
            print("Frontend discovery unavailable:", type(exc).__name__, flush=True)
        finally:
            browser.close()

    baseline = request(token, {})
    print("BASELINE:", json.dumps(baseline), flush=True)
    if not isinstance(baseline.get("total"), int) or baseline["total"] < 100000:
        raise RuntimeError("global search baseline invalid")
    extras = [
        {"pm": "1"}, {"pm": "true"},
        {"premium": "true"}, {"premiumOnly": "true"},
        {"premiumOnly": "1"}, {"pmo": "1"},
        {"membership": "Premium"}, {"membershipType": "Premium"},
        {"mt": "1"}, {"m": "1"},
        {"mo": "1"}, {"ms": "premium"},
        {"premium": "only"}, {"membership": "premiumonly"},
    ]
    positives = []
    for extra in extras:
        time.sleep(12.5)
        try:
            output = request(token, extra)
        except Exception as exc:
            output = {"error": str(type(exc).__name__)}
        label = urlencode(extra)
        print(f"TEST {label}: {json.dumps(output)}", flush=True)
        if (isinstance(output.get("total"), int)
                and 0 < output["total"] < baseline["total"]
                and output.get("pm_false") == 0
                and output.get("pm_true", 0) > 0):
            positives.append((label, output["total"]))
    print("VALIDATED_FILTER_CANDIDATES:", json.dumps(positives), flush=True)
    summary = os.getenv("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(f"## Premium-only worldwide search probe\\n\\n")
            f.write(f"Unfiltered worldwide: {baseline['total']:,}\\n\\n")
            f.write(f"Validated filter candidates: {positives}\\n")


if __name__ == "__main__":
    main()
