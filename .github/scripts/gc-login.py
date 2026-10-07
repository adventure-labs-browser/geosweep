"""Log into geocaching.com with real Chromium.

Datacenter form login is bot-walled; a full browser passes. The durable
output is gc-storage.json, whose session cookies let geosweep mint fresh
api-proxy bearers during long runs. A bearer is minted here only to prove
the saved session works; it is deliberately never exported.

Usage: GC_USER=... GC_PASS=... python3 gc-login.py
On failure: saves screenshot to gc-login-fail.png, prints page state.
"""
import json
import os
import sys
import traceback

from playwright.sync_api import sync_playwright

SIGNIN = "https://www.geocaching.com/account/signin?returnUrl=%2Fplay"
TOKEN = "https://www.geocaching.com/account/oauth/token"

user = os.environ["GC_USER"]
pw = os.environ["GC_PASS"]

CONSENT = [
    # Cookiebot (Usercentrics) — prefer necessary-only.
    'button:has-text("Necessary cookies only")',
    "#onetrust-accept-btn-handler",
    'button:has-text("Accept all")',
    'button:has-text("Accept")',
    'button:has-text("Agree")',
]


def fail(page, msg):
    try:
        url = page.url
    except Exception:
        url = "?"
    try:
        title = page.title()
    except Exception:
        title = "?"
    try:
        page.screenshot(path="gc-login-fail.png", full_page=False)
        shot = "screenshot saved"
    except Exception as e:
        shot = f"no screenshot: {e}"
    print(f"LOGIN FAILED: {msg} | url={url} | title={title} | {shot}",
          file=sys.stderr)
    print(traceback.format_exc(limit=3), file=sys.stderr)
    sys.exit(1)


with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    context = browser.new_context(
        user_agent="Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
    )
    page = context.new_page()
    try:
        page.goto(SIGNIN, wait_until="domcontentloaded", timeout=60000)
        page.wait_for_timeout(3000)
        for sel in CONSENT:
            try:
                btn = page.locator(sel).first
                if btn.count() and btn.is_visible():
                    btn.click(timeout=5000)
                    page.wait_for_timeout(1500)
                    break
            except Exception:
                pass
        page.locator('input[name="UsernameOrEmail"]').fill(user, timeout=15000)
        page.locator('input[name="Password"]').fill(pw, timeout=15000)
        # Submit via Enter: the page has multiple submit buttons (some
        # hidden) and a cookie overlay — all of which break button clicks.
        page.locator('input[name="Password"]').press("Enter")
        try:
            page.wait_for_url("**/play**", timeout=45000)
        except Exception:
            fail(page, "no redirect to /play after submit")
        resp = page.goto(TOKEN, timeout=60000)
        body = resp.text() if resp else ""
        # Persist the browser session. The scraper mints its own short-lived
        # bearers from these cookies and renews them before expiry.
        context.storage_state(path="gc-storage.json")
    except SystemExit:
        raise
    except Exception as e:
        fail(page, f"exception: {type(e).__name__} {str(e)[:150]}")
    finally:
        browser.close()

try:
    access = json.loads(body)["access_token"]
    assert access.count(".") == 2
except Exception:
    print(f"LOGIN FAILED: token endpoint gave: {body[:200]}", file=sys.stderr)
    sys.exit(1)

# Do not print or export the bearer. In particular, never put GC_BEARER in
# GITHUB_ENV: its presence would make a long run select non-renewable auth.
print("browser login ok; renewable session jar written")
