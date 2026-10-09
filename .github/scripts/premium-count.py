"""Count premium-only and regular geocaches with the website API's sp filter.

c:geo GCWebAPI WebApiSearch.setStatusMembership maps sp=1 to premium-only
and sp=0 to basic. Use one normal browser authentication, only two search
requests, and bounded Retry-After-aware throttling recovery.
"""
import json
import os
import subprocess
import sys
import time
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from urllib.error import HTTPError

API = "https://www.geocaching.com/api/proxy/web/search/v2"
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/126.0 Safari/537.36"


def count(token, membership):
    params = urlencode({"box": "90,-180,-90,180", "rad": "16000",
                        "take": 20, "skip": 0, "app": "geosweep",
                        "sp": membership})
    request = Request(f"{API}?{params}", headers={
        "Authorization": f"Bearer {token}",
        "Accept": "application/json", "User-Agent": UA,
    })
    for attempt in range(3):
        try:
            with urlopen(request, timeout=45) as response:
                body = json.load(response)
            break
        except HTTPError as err:
            if err.code != 429 or attempt == 2:
                raise RuntimeError(f"premium count HTTP {err.code}") from None
            retry = err.headers.get("Retry-After")
            try:
                delay = max(45, min(180, float(retry)))
            except (TypeError, ValueError):
                delay = 60 * (attempt + 1)
            print(f"Rate-limited, waiting {delay:.0f}s before retrying the same count query", flush=True)
            time.sleep(delay)
    total = body.get("total")
    results = body.get("results")
    if not isinstance(total, int) or total <= 0 or not isinstance(results, list):
        raise RuntimeError("Invalid filtered search response: missing total/results")
    premium = sum(x.get("premiumOnly") is True for x in results)
    regular = sum(x.get("premiumOnly") is False for x in results)
    if not results or premium + regular != len(results):
        raise RuntimeError("Search sample has missing premium flags")
    expected = membership == 1
    if premium != len(results) if expected else regular != len(results):
        raise RuntimeError(f"Filter sp={membership} ignored or misinterpreted")
    return total, len(results)


def main():
    helper = subprocess.run([
        sys.executable, ".github/scripts/gc-login.py",
        "--state", "gc-premium-count-state.json",
        "--screenshot", "/tmp/gc-premium-auth-fail.png",
    ], capture_output=True, text=True, timeout=140)
    if helper.returncode:
        print(helper.stderr[-750:], file=sys.stderr)
        raise RuntimeError("one-shot browser auth failed")
    token = json.loads(helper.stdout)["access_token"]
    # Only two count requests; no worldwide sweep and no database mutation.
    premium, sample1 = count(token, 1)
    print(f"WORLDWIDE PREMIUM-ONLY: {premium:,} (sample {sample1} confirmed premium)", flush=True)
    time.sleep(40)
    regular, sample0 = count(token, 0)
    print(f"WORLDWIDE REGULAR: {regular:,} (sample {sample0} confirmed non-premium)", flush=True)
    combined = premium + regular
    print(f"FILTERED TOTAL: {combined:,}", flush=True)
    # Previous independent API snapshot was 3,492,491 at 2026-10-09 17:55Z.
    print(f"DELTA FROM INDEPENDENT UNFILTERED 3,492,491: {combined - 3492491:+,}", flush=True)
    summary = os.getenv("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(f"## Worldwide cache membership counts (live API)\n\n")
            f.write(f"- Premium-only: **{premium:,}**\n")
            f.write(f"- Regular: **{regular:,}**\n")
            f.write(f"- Combined: **{combined:,}**\n")
            f.write(f"- Difference from earlier unfiltered count: {combined - 3492491:+,}\n")


if __name__ == "__main__":
    main()
