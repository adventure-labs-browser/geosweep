#!/usr/bin/env python3
"""Run the normal bounded GeoSweep refresh and support a remote soft-stop.

A request is a short-lived Git ref named geosweep-stop-<GITHUB_RUN_ID>.
This only reads GitHub metadata, not the geocaching website. On request,
send SIGTERM to GNU timeout, which forwards it to the Rust crawler.
After the process has exited, the Actions workflow validates and publishes
the SQLite checkpoint as on a normal budget stop.
"""
import os
import signal
import subprocess
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

POLL_SECS = 20
REQUEST_TIMEOUT_SECS = 10
STOP_GRACE_SECS = 150


def control_ref(repo, run_id):
    if not repo or not run_id.isdecimal():
        raise ValueError("Expected GITHUB_REPOSITORY and numeric GITHUB_RUN_ID")
    return "repos/{}/git/ref/heads/geosweep-stop-{}".format(repo, run_id)


def stop_requested(repo, run_id, token):
    if not token:
        raise RuntimeError("GH_TOKEN is required for soft-stop checks")
    url = "https://api.github.com/" + control_ref(repo, run_id)
    req = Request(url, headers={
        "Authorization": "Bearer " + token,
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "geosweep-controlled-refresh",
    })
    try:
        with urlopen(req, timeout=REQUEST_TIMEOUT_SECS) as response:
            return response.status == 200
    except HTTPError as err:
        if err.code == 404:
            return False
        raise


def main():
    if len(sys.argv) != 3:
        print("Usage: controlled-refresh.py <budget-secs> <starting-rate>", file=sys.stderr)
        return 2
    budget = int(sys.argv[1])
    rate = float(sys.argv[2])
    if not 60 <= budget <= 17100 or not (0 < rate <= 20):
        raise ValueError("Invalid crawl budget or initial rate")
    repo = os.environ["GITHUB_REPOSITORY"]
    run_id = os.environ["GITHUB_RUN_ID"]
    token = os.environ["GH_TOKEN"]
    control_ref(repo, run_id)

    timeout_bin = "gtimeout" if os.getenv("RUNNER_OS") == "macOS" else "timeout"
    command = [
        timeout_bin, "--preserve-status", "--signal=TERM",
        "--kill-after=5m", "315m",
        "./target/release/geosweep", "--db", "data/geosweep.db",
        "refresh", "--crawl-rate", str(rate), "--crawl-budget-secs", str(budget),
    ]
    # Don't expose the repository API token or unused static auth credentials
    # to the crawler/browser subprocess.
    env = os.environ.copy()
    for key in ("GH_TOKEN", "GC_BEARER", "GC_JAR"):
        env.pop(key, None)

    child = subprocess.Popen(command, env=env)
    asked_to_stop = False
    deadline = None
    next_check = time.monotonic() + POLL_SECS
    print("soft-stop watcher active for run {}; checking every {}s".format(
        run_id, POLL_SECS), flush=True)
    try:
        while child.poll() is None:
            now = time.monotonic()
            if now >= next_check:
                next_check = now + POLL_SECS
                try:
                    if stop_requested(repo, run_id, token):
                        asked_to_stop = True
                        deadline = now + STOP_GRACE_SECS
                        print("SOFT STOP REQUESTED: forwarding SIGTERM and saving checkpoint",
                              flush=True)
                        child.send_signal(signal.SIGTERM)
                        break
                except (HTTPError, URLError, TimeoutError, OSError) as err:
                    print("soft-stop GitHub check unavailable: {}; will retry".format(
                        type(err).__name__), file=sys.stderr, flush=True)
            time.sleep(0.5)
        if asked_to_stop:
            try:
                remaining = max(1, deadline - time.monotonic())
                child.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                print("soft stop did not exit within grace period; terminating job",
                      file=sys.stderr, flush=True)
                # Don't claim a successful checkpoint. The remaining
                # workflow still runs DB validation with if: always().
                child.kill()
                child.wait()
                return 1
        rc = child.wait()
        if asked_to_stop and rc in (0, 130, 143):
            print("SOFT STOP COMPLETE: crawler exited; continuing to DB validation/publication",
                  flush=True)
            return 0
        return rc
    except BaseException:
        if child.poll() is None:
            child.terminate()
        raise


if __name__ == "__main__":
    sys.exit(main())
