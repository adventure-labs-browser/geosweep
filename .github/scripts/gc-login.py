"""Self-healing browser authentication for geosweep.

This helper owns the browser session. It first tries the saved Playwright
storage state, falls back to a fresh interactive-form login when that session
is stale, verifies the resulting session by minting a website API bearer, and
prints exactly one JSON token object to stdout.

All diagnostics go to stderr. The caller captures stdout, so bearer values
never reach GitHub Actions logs.
"""
import argparse
import base64
import json
import os
import sys
import time
import traceback
from pathlib import Path

from playwright.sync_api import sync_playwright

SIGNIN = "https://www.geocaching.com/account/signin?returnUrl=%2Fplay"
TOKEN = "https://www.geocaching.com/account/oauth/token"
USER_AGENT = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/153.0 Safari/537.36"
)

CONSENT = [
    'button:has-text("Necessary cookies only")',
    "#onetrust-accept-btn-handler",
    'button:has-text("Accept all")',
    'button:has-text("Accept")',
    'button:has-text("Agree")',
]


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def jwt_remaining_seconds(token):
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        data = json.loads(base64.urlsafe_b64decode(payload.encode()))
        exp = int(data["exp"])
        return max(1, exp - int(time.time()))
    except Exception:
        return None


def parse_token_response(resp):
    if resp is None:
        raise RuntimeError("token endpoint returned no response")
    status = resp.status
    body = resp.text()
    if status != 200:
        raise RuntimeError(f"token endpoint HTTP {status}: {body[:160]!r}")
    try:
        data = json.loads(body)
    except Exception as e:
        # Do not dump pages/cookies into Actions logs. A redirect to a sign-in
        # or challenge page is a useful auth diagnostic, unlike HTML fragments.
        import re
        from urllib.parse import urlsplit

        title = re.search(r"<title[^>]*>(.*?)</title>", body, re.I | re.S)
        title_text = re.sub(r"\s+", " ", title.group(1)).strip()[:100] if title else "(none)"
        final_path = urlsplit(resp.url).path
        raise RuntimeError(
            f"token endpoint returned non-JSON: HTTP {status}, "
            f"final_path={final_path!r}, html_title={title_text!r}"
        ) from e
    access = data.get("access_token")
    if not isinstance(access, str) or access.count(".") != 2:
        raise RuntimeError("token endpoint response has no JWT access_token")
    remaining = data.get("expires_in")
    if not isinstance(remaining, (int, float)) or remaining <= 0:
        remaining = jwt_remaining_seconds(access) or 3600
    return access, int(remaining)


def mint(context):
    # Use a disposable page so probing the token endpoint never navigates the
    # login page away from its form. This is intentionally a real browser
    # navigation rather than a bare HTTP request because datacenter traffic is
    # treated differently by the site.
    token_page = context.new_page()
    try:
        resp = token_page.goto(TOKEN, wait_until="domcontentloaded", timeout=60000)
        return parse_token_response(resp)
    finally:
        token_page.close()


def dismiss_consent(page):
    for sel in CONSENT:
        try:
            btn = page.locator(sel).first
            if btn.count() and btn.is_visible():
                btn.click(timeout=5000)
                page.wait_for_timeout(750)
                return
        except Exception:
            pass


def fresh_login(context, page, user, password):
    page.goto(SIGNIN, wait_until="domcontentloaded", timeout=60000)
    page.wait_for_timeout(1500)
    dismiss_consent(page)
    user_field = page.locator('input[name="UsernameOrEmail"]').first
    pass_field = page.locator('input[name="Password"]').first
    user_field.wait_for(state="visible", timeout=20000)
    pass_field.wait_for(state="visible", timeout=20000)
    user_field.fill(user)
    pass_field.fill(password)

    # Enter is the least brittle primary submit path. If the page does not
    # advance, try the first visible submit button as a fallback.
    pass_field.press("Enter")
    try:
        page.wait_for_url("**/play**", timeout=30000)
    except Exception:
        for selector in ['button[type="submit"]', 'input[type="submit"]']:
            try:
                buttons = page.locator(selector)
                for i in range(buttons.count()):
                    button = buttons.nth(i)
                    if button.is_visible():
                        button.click(timeout=5000)
                        raise StopIteration
            except StopIteration:
                break
            except Exception:
                pass
        try:
            page.wait_for_url("**/play**", timeout=20000)
        except Exception:
            # The token endpoint is the authoritative login check. Some site
            # variants do not land on /play even though authentication worked.
            pass

    return mint(context)


def save_state(context, state_path):
    path = Path(state_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    context.storage_state(path=str(tmp))
    os.replace(tmp, path)


def make_context(browser, state_path):
    kwargs = {"user_agent": USER_AGENT}
    if Path(state_path).is_file():
        try:
            kwargs["storage_state"] = state_path
            return browser.new_context(**kwargs), True
        except Exception as e:
            log(f"saved browser state could not be loaded; rebuilding it: {e}")
            kwargs.pop("storage_state", None)
    return browser.new_context(**kwargs), False


def run_once(state_path, user, password, screenshot):
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        context, had_state = make_context(browser, state_path)
        page = context.new_page()
        try:
            if had_state:
                try:
                    access, expires_in = mint(context)
                    save_state(context, state_path)
                    return access, expires_in, "saved-session"
                except Exception as e:
                    log(f"saved session no longer mints a bearer; rebuilding it: {e}")
                    page.close()
                    context.close()
                    context = browser.new_context(user_agent=USER_AGENT)
                    page = context.new_page()

            access, expires_in = fresh_login(context, page, user, password)
            save_state(context, state_path)
            return access, expires_in, "fresh-login"
        except Exception:
            try:
                page.screenshot(path=screenshot, full_page=False)
            except Exception:
                pass
            raise
        finally:
            context.close()
            browser.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--state", default=os.environ.get("GC_BROWSER_STATE", "gc-storage.json"))
    ap.add_argument("--screenshot", default="gc-login-fail.png")
    args = ap.parse_args()

    user = os.environ.get("GC_USER", "")
    password = os.environ.get("GC_PASS", "")
    if not user or not password:
        log("AUTH CONFIG ERROR: GC_USER and GC_PASS are required")
        return 2

    try:
        access, expires_in, source = run_once(args.state, user, password, args.screenshot)
    except Exception as e:
        log(f"AUTH ATTEMPT FAILED: {type(e).__name__}: {e}")
        log(traceback.format_exc(limit=4))
        return 1

    # stdout is a machine-only channel captured by the Rust parent.
    print(json.dumps({
        "access_token": access,
        "expires_in": expires_in,
        "source": source,
    }), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
