"""Read global geocache search total in three small, authenticated API calls.

No crawler, no queue mutation, and no token stored in artifacts or logs.
"""
import json
import os
import sqlite3
import subprocess
import sys
import time
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

ENDPOINT = "https://www.geocaching.com/api/proxy/web/search/v2"
USER_AGENT = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
    "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
)


def query_count(bearer, box):
    params = urlencode({
        "box": box, "rad": "16000", "take": "1", "skip": "0", "app": "geosweep"
    })
    request = Request(
        f"{ENDPOINT}?{params}",
        headers={
            "Authorization": f"Bearer {bearer}",
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        },
    )
    try:
        with urlopen(request, timeout=60) as response:
            payload = json.load(response)
    except HTTPError as exc:
        raise RuntimeError(f"search endpoint returned HTTP {exc.code}") from None

    total = payload.get("total")
    results = payload.get("results")
    if type(total) is not int or total <= 0 or not isinstance(results, list):
        raise RuntimeError("search response missing positive integer total or results array")
    if len(results) > 1:
        raise RuntimeError("take=1 returned multiple records")
    return total


def main():
    helper = subprocess.run(
        [sys.executable, ".github/scripts/gc-login.py", "--state", "gc-world-count-state.json"],
        capture_output=True, text=True, timeout=150, check=False,
    )
    if helper.returncode:
        # Browser helper messages intentionally redact credentials and bearer.
        print(helper.stderr[-1000:], file=sys.stderr)
        raise RuntimeError("one-shot browser authentication failed")
    token = json.loads(helper.stdout)["access_token"]
    # Global query is the actual count. Two hemisphere queries independently
    # test interpretation of the world bounding box and response total.
    world = query_count(token, "90,-180,-90,180")
    print(f"Geocaching search worldwide total: {world:,}", flush=True)
    others = {}
    for name, box in [
        ("Western hemisphere", "90,-180,-90,0"),
        ("Eastern hemisphere", "90,0,-90,180"),
    ]:
        time.sleep(12)  # Respect the ongoing crawler's learned low rate.
        try:
            others[name] = query_count(token, box)
            print(f"{name}: {others[name]:,}", flush=True)
        except Exception as exc:
            print(f"{name}: validation unavailable ({type(exc).__name__}: {exc})")
    sum_halves = sum(others.values()) if len(others) == 2 else None
    if sum_halves is not None:
        print(f"Hemispheres combined: {sum_halves:,} (difference {sum_halves-world:+,})")
        if abs(sum_halves - world) > max(1000, world * 0.01):
            print("WARNING: bounding-box counts disagree; treat worldwide count as unverified")

    archive = Path("data/geosweep.db")
    archived = None
    if archive.exists():
        with sqlite3.connect(f"file:{archive}?mode=ro", uri=True) as connection:
            archived = connection.execute("SELECT COUNT(*) FROM caches").fetchone()[0]
        print(f"Archived records: {archived:,}")
        if world:
            print(f"Archive / live-search total (rough): {100 * archived / world:.2f}%")

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as out:
            out.write(f"## Geocaching worldwide search count\n\n**{world:,}** results.\n\n")
            if sum_halves is not None:
                out.write(f"Hemispheres: {sum_halves:,} combined (difference {sum_halves-world:+,}).\n\n")
            if archived is not None:
                out.write(f"Archived records: {archived:,} (~{100*archived/world:.2f}% of live result count).\n")
            out.write("\nAPI totals represent matching searchable caches, not historical/deleted caches.\n")


if __name__ == "__main__":
    main()
