"""Log into geocaching.com with real Chromium (datacenter form login
is bot-walled; a full browser passes) and print an api-proxy bearer.

Usage: GC_USER=... GC_PASS=... python3 gc-login.py
Stdout line 1: ::add-mask::<token> (caller echoes to step stdout)
Stdout line 2: GC_BEARER=<token> (caller appends to $GITHUB_ENV)
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
    page = browser.new_page(
        user_agent="Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
    )
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

# Mask BEFORE the value ever hits stdout.
print(f"::add-mask::{access}")
print(f"GC_BEARER={access}")
