#!/usr/bin/env python3
"""Small authenticated Oracle egress rate probe. Never logs tokens or cache data."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

URL = "https://www.geocaching.com/api/proxy/web/search/v2"
AGENT = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
         "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
BOXES = ["38,-123,37,-122", "41,-75,40,-73", "52,-1,51,1", "36,138,35,140"]
STAGES = [(0.15, 12), (0.30, 30), (0.60, 36), (1.00, 20)]
MAX_CALLS = sum(n for _, n in STAGES)


def get_token():
    with tempfile.TemporaryDirectory(prefix="geo-oracle-auth-") as tmp:
        args = [sys.executable, ".github/scripts/gc-login.py",
                "--state", str(Path(tmp) / "state.json"),
                "--screenshot", str(Path(tmp) / "failure.png")]
        try:
            result = subprocess.run(args, capture_output=True, text=True,
                                    timeout=180, check=True)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
            print("AUTH_FAILED", type(exc).__name__, file=sys.stderr, flush=True)
            raise SystemExit(2) from None
        parsed = json.loads(result.stdout)
        token = parsed["access_token"]
        if token.count(".") != 2:
            raise RuntimeError("No valid website bearer received")
        print("AUTH_OK; token securely held in memory", flush=True)
        return token


def measure(token):
    ok = limited = failures = 0
    times = []
    started = time.monotonic()
    print("ORACLE_EGRESS_PROBE max_calls={} starts={}".format(MAX_CALLS, time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())), flush=True)
    count = 0
    for rate, attempts in STAGES:
        print("STAGE_START requested_rps={:.2f} max_requests={}".format(rate, attempts), flush=True)
        stage_ok = 0
        stage_start = time.monotonic()
        next_start = stage_start
        for i in range(attempts):
            remaining = next_start - time.monotonic()
            if remaining > 0:
                time.sleep(remaining)
            next_start = time.monotonic() + 1 / rate
            box = BOXES[count % len(BOXES)]
            count += 1
            url = "{}?box={}&rad=16000&take=1&skip=0&sort=distance&asc=true&app=geosweep".format(URL, box)
            req = Request(url, headers={"Authorization": "Bearer " + token, "Accept": "application/json", "User-Agent": AGENT})
            begin = time.monotonic()
            try:
                with urlopen(req, timeout=25) as response:
                    status = response.status
                    headers = response.headers
                    response.read(2048)
            except HTTPError as err:
                status = err.code
                headers = err.headers
            except (URLError, TimeoutError, OSError) as err:
                failures += 1
                print("STOP network_error={} at_request={}".format(type(err).__name__, count), flush=True)
                return 3
            elapsed_ms = round((time.monotonic() - begin) * 1000)
            times.append(elapsed_ms)
            if status == 200:
                ok += 1
                stage_ok += 1
            else:
                if status == 429:
                    limited += 1
                else:
                    failures += 1
                print("STOP status={} at_request={} stage_rps={:.2f} stage_ok={} retry_after={} x_ratelimit_remaining={} rate_limit_remaining={}".format(
                    status, count, rate, stage_ok, headers.get("Retry-After"),
                    headers.get("X-RateLimit-Remaining"), headers.get("RateLimit-Remaining")), flush=True)
                print("SUMMARY requests={} ok={} http429={} other_fail={} elapsed_s={:.1f}".format(
                    count, ok, limited, failures, time.monotonic()-started), flush=True)
                return 0 if status == 429 else 4
        print("STAGE_DONE requested_rps={:.2f} ok={} elapsed_s={:.1f}".format(
            rate, stage_ok, time.monotonic()-stage_start), flush=True)
    print("SUMMARY requests={} ok={} http429={} other_fail={} elapsed_s={:.1f} median_ms={}".format(
        count, ok, limited, failures, time.monotonic()-started, sorted(times)[len(times)//2] if times else None), flush=True)
    print("PROBE_LIMIT_REACHED: no 429 observed; no claim about long-term rate", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(measure(get_token()))
