"""Measure premium-only cache count via Geocaching search membership filter.
The statusMembership mapping in c:geo's GCWebAPI.java is sp=1 for premium,
sp=0 for regular. V2 and V1 endpoints are tested with full query parameters.
"""
import json
import os
import subprocess
import sys
import time
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

ROOT = "https://www.geocaching.com/api/proxy"
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/126.0 Safari/537.36"
SNAPSHOT = 3492491  # unfiltered Oct 9 2026 authenticated count


def query(token, endpoint, membership):
    params = urlencode({
        "box": "90,-180,-90,180", "origin": "0,0", "rad": "16000",
        "take": 20, "skip": 0, "sort": "distance", "asc": "true",
        "properties": "callernote", "app": "geosweep", "sp": membership,
    })
    req = Request(f"{ROOT}{endpoint}?{params}", headers={
        "Authorization": f"Bearer {token}", "Accept": "application/json",
        "User-Agent": UA,
    })
    for attempt in range(3):
        try:
            with urlopen(req, timeout=45) as response:
                data = json.load(response)
            break
        except HTTPError as exc:
            if exc.code != 429 or attempt == 2:
                return {"error": f"HTTP {exc.code}"}
            try:
                delay = max(60, min(180, float(exc.headers.get("Retry-After"))))
            except (TypeError, ValueError):
                delay = 65 * (attempt + 1)
            print(f"{endpoint} sp={membership}: 429, backoff {delay:.0f}s", flush=True)
            time.sleep(delay)
    rows = data.get("results", [])
    total = data.get("total")
    flags = [r.get("premiumOnly") for r in rows]
    correct = bool(rows) and all(x is (membership == 1) for x in flags)
    output = {"total": total, "sample": len(rows),
              "premium_sample": sum(x is True for x in flags),
              "regular_sample": sum(x is False for x in flags),
              "valid_filter": correct}
    print(f"{endpoint} sp={membership}: {json.dumps(output)}", flush=True)
    return output


def main():
    helper = subprocess.run([
        sys.executable, ".github/scripts/gc-login.py",
        "--state", "gc-premium-count-state.json",
        "--screenshot", "/tmp/gc-premium-auth-fail.png",
    ], capture_output=True, text=True, timeout=140)
    if helper.returncode:
        print(helper.stderr[-700:], file=sys.stderr)
        raise RuntimeError("one-shot browser sign-in failed")
    token = json.loads(helper.stdout)["access_token"]
    for endpoint in ("/web/search/v2", "/web/search"):
        premium = query(token, endpoint, 1)
        if not premium.get("valid_filter"):
            print(f"{endpoint}: premium-only filter not supported in this session", flush=True)
            time.sleep(45)
            continue
        time.sleep(45)
        basic = query(token, endpoint, 0)
        if not basic.get("valid_filter"):
            print(f"{endpoint}: unable to validate regular-only filter", flush=True)
            time.sleep(45)
            continue
        p, b = premium["total"], basic["total"]
        if not all(isinstance(x, int) and x > 0 for x in (p, b)):
            raise RuntimeError("Filtered query lacks positive numeric total")
        diff = p + b - SNAPSHOT
        print(f"PREMIUM-ONLY WORLDWIDE: {p:,}", flush=True)
        print(f"REGULAR WORLDWIDE: {b:,}", flush=True)
        print(f"COMBINED: {p + b:,} vs independent baseline {SNAPSHOT:,}, difference {diff:+,}", flush=True)
        summary = os.getenv("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a") as fp:
                fp.write(f"## Geocache membership counts ({endpoint})\\n\\n")
                fp.write(f"Premium-only: **{p:,}**\\n\\n")
                fp.write(f"Regular: **{b:,}**\\n\\n")
                fp.write(f"Combined: {p+b:,} (difference {diff:+,} vs earlier baseline)\\n")
        if abs(diff) > 500:
            raise RuntimeError("Filter totals disagree with worldwide baseline")
        return
    raise RuntimeError("Neither API endpoint honored premium-only filtering for this account")


if __name__ == "__main__":
    main()
