"""Log into geocaching.com with real Chromium (datacenter form login
is bot-walled; a full browser passes) and print an api-proxy bearer.

Usage: GC_USER=... GC_PASS=... python3 gc-login.py >> $GITHUB_ENV
Prints: GC_BEARER=<token> (caller must ::add-mask:: it first — we do
that here ourselves before printing the value).
"""
import json
import os
import sys

from playwright.sync_api import sync_playwright

SIGNIN = "https://www.geocaching.com/account/signin?returnUrl=%2Fplay"
TOKEN = "https://www.geocaching.com/account/oauth/token"

user = os.environ["GC_USER"]
pw = os.environ["GC_PASS"]

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page(
        user_agent="Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
    )
    page.goto(SIGNIN, wait_until="domcontentloaded", timeout=60000)
    page.locator('input[name="UsernameOrEmail"]').fill(user)
    page.locator('input[name="Password"]').fill(pw)
    page.locator('input[type="submit"], button[type="submit"]').first.click()
    page.wait_for_url("**/play**", timeout=60000)
    tok = page.goto(TOKEN, timeout=60000)
    body = tok.text() if tok else ""
    browser.close()

try:
    access = json.loads(body)["access_token"]
    assert access.count(".") == 2
except Exception:
    print(f"login failed, token endpoint gave: {body[:200]}", file=sys.stderr)
    sys.exit(1)

# Mask BEFORE the value ever hits stdout.
print(f"::add-mask::{access}")
print(f"GC_BEARER={access}")
