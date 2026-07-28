#!/usr/bin/env python3
"""
Brute-force template discovery for partial-supported event-based titles.

Strategy: every partial title in the bundle has a per-achievement
"replacement" template like:

    { "Replacement": { "ReplacementType": "Replace",
                       "Target": "REPLACEINDEX",
                       "Replacement": <int> } }

The community has only mapped some achievements. The mapping is
{ ach_id -> internal_progression_index }. We discover the unmapped ones
by:

  1. fetching the baseline of currently-unlocked achievements
  2. for N = 1..MAX (skipping known mappings):
     a. render the title's template with REPLACEINDEX (or REPLACEID etc)=N
     b. POST to OneCollector
     c. re-fetch achievements
     d. diff -> if a new ach unlocked, map (N -> new_ach_id)
  3. save discovered mappings to discovered/<title_id>.json

Limits: only works for "single-replacement" templates. Multi-field
templates (Halo MCC etc) need Frida capture from a real game session.

Run:
  python scripts/discover_templates.py [--title <id>] [--all-simple]
                                        [--max-n 100] [--delay 1.5]
"""

import argparse
import datetime
import json
import os
import sys
import time
from pathlib import Path

import requests
# OneCollector uses Microsoft's PKI which Python's bundled bundle on
# some Windows installs lacks the root for. Disable verification —
# we're only POSTing telemetry payloads (no secrets in body), and
# the events_xauth header is the actual auth gate. Same trade-off
# the C# upstream makes.
import urllib3
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

# ---------- xau-rs constants pinned from src/events/unlock.rs ----------
API_KEY = (
    "0890af88a9ed4cc886a14f5e174a2827-9de66c5e-f867-43a8-a7b8-e0ddd481cca4-7548,"
    "95c1f21d6cb047a09e7b423c1cb2222e-9965f07b-54fa-498e-9727-9e8d24dec39e-7027"
)
CLIENT_VERSION = (
    "EUTC-Windows-C++-no-10.0.22621.3296.amd64fre.ni_release.220506-1250-no"
)
TELEMETRY_URL = "https://v20.events.data.microsoft.com/OneCollector/1.0/"
ACHIEVEMENTS_URL = (
    "https://achievements.xboxlive.com/users/xuid({xuid})/achievements"
    "?titleId={tid}&maxItems=1000"
)
PROFILE_URL = "https://profile.xboxlive.com/users/me/profile/settings"

XAU_DIR = Path(os.environ.get("USERPROFILE", os.path.expanduser("~"))) / "Documents" / "XAU"
EVENTS_DIR = XAU_DIR / "Events"
DISCOVERED_DIR = EVENTS_DIR / "discovered"
DATA_JSON = EVENTS_DIR / "Data.json"
AUTH_JSON = XAU_DIR / "auth.json"

# Standard placeholders that aren't the brute-force target
STANDARD_PLACEHOLDERS = {"REPLACETIME", "REPLACESEQ", "REPLACEXUID"}


def load_auth():
    with open(AUTH_JSON, encoding="utf-8") as f:
        d = json.load(f)
    xauth = d.get("xauth")
    events_xauth = d.get("events_xauth")
    if not xauth or not events_xauth:
        sys.exit("auth.json missing xauth or events_xauth — run device-code login first")
    return xauth, f"x:{events_xauth}"


def get_xuid(xauth):
    """Try profile endpoint first; fall back to decoding the JWT in xauth.

    Profile endpoint can be 429-throttled from prior bbb runs — the JWT
    fallback uses zero network calls and still gives us the correct XUID
    via the `xid` claim minted by Microsoft at login time.
    """
    try:
        r = requests.get(
            PROFILE_URL,
            headers={
                "Authorization": xauth,
                "x-xbl-contract-version": "3",
                "Accept": "application/json",
                "Accept-Language": "en-US",
            },
            params={"settings": "Gamertag"},
            timeout=20,
        )
        if r.status_code == 200:
            return r.json()["profileUsers"][0]["id"]
    except Exception as e:
        print(f"    [profile endpoint failed: {e}] — falling back to JWT decode")

    # Fallback: extract xid from JWT in xauth
    import base64
    if not xauth.startswith("XBL3.0 x="):
        sys.exit(f"can't parse xauth shape: {xauth[:30]}")
    rest = xauth[len("XBL3.0 x="):]
    semi = rest.find(";")
    if semi < 0:
        sys.exit("xauth has no JWT payload separator")
    jwt = rest[semi+1:]
    parts = jwt.split(".")
    if len(parts) < 2:
        sys.exit("malformed JWT in xauth")
    payload = parts[1]
    # Pad base64
    payload += "=" * (-len(payload) % 4)
    try:
        claims = json.loads(base64.urlsafe_b64decode(payload))
    except Exception as e:
        sys.exit(f"can't decode JWT: {e}")
    # XBL JWTs put display claims under xid or xui[0].xid
    xid = claims.get("xid")
    if not xid and isinstance(claims.get("xui"), list) and claims["xui"]:
        xid = claims["xui"][0].get("xid")
    # Some shapes use uppercase Xid
    if not xid:
        xid = claims.get("Xid")
    if not xid and isinstance(claims.get("DisplayClaims", {}).get("xui", []), list):
        x = claims["DisplayClaims"]["xui"]
        if x and isinstance(x[0], dict):
            xid = x[0].get("xid")
    if not xid:
        sys.exit(f"JWT has no xid claim. claims keys: {list(claims.keys())}")
    return xid


def get_achievements(xauth, xuid, title_id):
    """Returns dict ach_id -> {'unlocked': bool, 'name': str, 'gs': int}."""
    r = requests.get(
        ACHIEVEMENTS_URL.format(xuid=xuid, tid=title_id),
        headers={
            "Authorization": xauth,
            "x-xbl-contract-version": "4",
            "Accept": "application/json",
            "Accept-Language": "en-US",
        },
        timeout=30,
    )
    if r.status_code == 429:
        retry_s = float(r.headers.get("Retry-After", "5"))
        print(f"    [scan throttled, sleeping {retry_s:.1f}s]")
        time.sleep(retry_s + 1)
        return get_achievements(xauth, xuid, title_id)
    r.raise_for_status()
    j = r.json()
    out = {}
    for a in j.get("achievements", []):
        ach_id = a.get("id")
        progress = a.get("progressState", "")
        unlocked = progress.lower() == "achieved"
        rewards = a.get("rewards") or []
        gs = 0
        for rw in rewards:
            if rw.get("type") == "Gamerscore":
                try:
                    gs = int(rw.get("value", 0))
                except (TypeError, ValueError):
                    pass
        out[ach_id] = {"unlocked": unlocked, "name": a.get("name", ""), "gs": gs}
    return out


def blank_user_hash(xauth):
    """`XBL3.0 x=USERHASH;JWT` -> `XBL3.0 x=-;JWT`"""
    if xauth.startswith("XBL3.0 x="):
        rest = xauth[len("XBL3.0 x="):]
        idx = rest.find(";")
        if idx >= 0:
            return f"XBL3.0 x=-;{rest[idx+1:]}"
    return xauth


def iso_utc_fractional_now():
    now = datetime.datetime.now(datetime.timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond * 10:07d}Z"


def render_event(template_str, placeholder, value, xuid):
    """Standard renderer mimicking xau-core/events/bundle.rs.

    NOTE: OneCollector rejects formatted JSON with `InvalidEventFormat`.
    The template files are pretty-printed for human readability — we have
    to compact (strip whitespace) the final body. The Rust client's
    serde_json::to_string serializes objects compactly by default; that's
    why xau-core unlocks succeed with the same template.
    """
    body = template_str.replace(placeholder, str(value))
    body = body.replace("REPLACETIME", iso_utc_fractional_now())
    body = body.replace("REPLACESEQ", str(int(time.time())))
    body = body.replace("REPLACEXUID", str(xuid))
    # Re-serialize compactly so OneCollector accepts it
    return json.dumps(json.loads(body), separators=(",", ":"))


def post_event(body, xauth, events_xauth):
    r = requests.post(
        TELEMETRY_URL,
        data=body,
        headers={
            "user-agent": "MSDW",
            "cache-control": "no-cache",
            "accept": "application/json",
            "accept-encoding": "gzip, deflate",
            "reliability-mode": "standard",
            "client-version": CLIENT_VERSION,
            "apikey": API_KEY,
            "Client-Id": "NO_AUTH",
            "host": "v20.events.data.microsoft.com",
            "connection": "close",
            "authxtoken": blank_user_hash(xauth),
            "tickets": f'"1"="{events_xauth}"',
            "content-type": "application/x-json-stream",
        },
        timeout=30,
        verify=False,
    )
    return r.status_code, r.text


def find_placeholder(template_str):
    import re
    placeholders = set(re.findall(r"REPLACE[A-Z][A-Z0-9]*", template_str))
    custom = placeholders - STANDARD_PLACEHOLDERS
    if len(custom) == 1:
        return next(iter(custom))
    return None  # multi-field — not supported by simple brute force


def discover_title(title_id, xauth, events_xauth, xuid, max_n=100, delay=1.5,
                   stop_after_consecutive_misses=8):
    template_path = EVENTS_DIR / f"{title_id}.json"
    if not template_path.exists():
        return {"error": f"no template file for {title_id}"}
    template_str = template_path.read_text(encoding="utf-8")

    placeholder = find_placeholder(template_str)
    if not placeholder:
        return {"error": "multi-field template — use Frida capture instead"}

    # Load existing mapping from bundle
    with open(DATA_JSON, encoding="utf-8") as f:
        data = json.load(f)
    title = data.get(title_id, {})
    existing = {}  # ach_id -> int
    achs = (title or {}).get("Achievements") if isinstance(title, dict) else None
    if isinstance(achs, dict):
        for aid, av in achs.items():
            if not isinstance(av, dict):
                continue
            for k, v in av.items():
                if isinstance(v, dict) and v.get("Target") == placeholder:
                    try:
                        existing[aid] = int(str(v.get("Replacement")))
                    except (TypeError, ValueError):
                        pass
    known_ns = set(existing.values())

    print(f"[{title_id}] template placeholder = {placeholder}")
    print(f"[{title_id}] {len(existing)} achievements already mapped (Ns: {sorted(known_ns)[:20]}...)")

    # Baseline state
    print(f"[{title_id}] fetching baseline achievements...")
    baseline = get_achievements(xauth, xuid, title_id)
    if not baseline:
        return {"error": "no achievements returned for title"}
    total_achs = len(baseline)
    initially_unlocked = sum(1 for v in baseline.values() if v["unlocked"])
    print(f"[{title_id}] {total_achs} total achievements, {initially_unlocked} already unlocked")

    # Brute force
    discovered = {}  # ach_id -> int (new mappings only)
    state = baseline.copy()
    consecutive_misses = 0

    for n in range(1, max_n + 1):
        if n in known_ns:
            continue  # skip already-mapped Ns
        # POST event
        body = render_event(template_str, placeholder, n, xuid)
        status, text = post_event(body, xauth, events_xauth)
        if status >= 400:
            if status == 429:
                print(f"    [n={n}] OneCollector throttled (429); sleeping 15s")
                time.sleep(15)
                continue
            print(f"    [n={n}] POST error {status}: {text[:200]}")
            consecutive_misses += 1
            if consecutive_misses >= stop_after_consecutive_misses:
                print(f"[{title_id}] {consecutive_misses} consecutive errors — stopping")
                break
            continue

        time.sleep(delay)

        # Re-fetch achievements
        try:
            current = get_achievements(xauth, xuid, title_id)
        except Exception as e:
            print(f"    [n={n}] scan error: {e}; pausing 10s")
            time.sleep(10)
            continue

        # Diff: any new unlocks?
        new_unlocks = [
            aid for aid, v in current.items()
            if v["unlocked"] and not state.get(aid, {}).get("unlocked", False)
        ]
        if new_unlocks:
            for aid in new_unlocks:
                if aid not in existing and aid not in discovered:
                    discovered[aid] = n
                    name = current[aid]["name"]
                    gs = current[aid]["gs"]
                    print(f"  ✓ n={n:>3} → ach {aid} ('{name[:40]}', {gs}G)")
            state = current
            consecutive_misses = 0
        else:
            consecutive_misses += 1
            if consecutive_misses % 5 == 0:
                print(f"    [n={n}] {consecutive_misses} consecutive misses (no new unlock)")
            if consecutive_misses >= stop_after_consecutive_misses:
                print(f"[{title_id}] {consecutive_misses} consecutive misses — stopping at n={n}")
                break

        # Throttle ourselves modestly
        time.sleep(0.3)

    # Save discoveries
    if discovered:
        out_path = DISCOVERED_DIR / f"{title_id}.json"
        out_path.parent.mkdir(parents=True, exist_ok=True)
        result = {
            "title_id": title_id,
            "placeholder": placeholder,
            "discovered_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "max_n_tried": n,
            "mappings": {str(aid): val for aid, val in sorted(discovered.items(), key=lambda x: x[1])},
        }
        with open(out_path, "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2)
        print(f"[{title_id}] saved {len(discovered)} new mappings to {out_path}")

    return {
        "title_id": title_id,
        "placeholder": placeholder,
        "existing_count": len(existing),
        "discovered_count": len(discovered),
        "max_n_tried": n,
        "discovered": discovered,
    }


def merge_into_bundle():
    """Read discovered/*.json and merge into Data.json."""
    if not DISCOVERED_DIR.exists():
        return
    with open(DATA_JSON, encoding="utf-8") as f:
        data = json.load(f)
    merged = 0
    for f in DISCOVERED_DIR.glob("*.json"):
        with open(f, encoding="utf-8") as fp:
            disc = json.load(fp)
        tid = disc["title_id"]
        placeholder = disc["placeholder"]
        title = data.get(tid)
        if not isinstance(title, dict):
            continue
        if "Achievements" not in title or not isinstance(title["Achievements"], dict):
            title["Achievements"] = {}
        for aid, n in disc["mappings"].items():
            if aid in title["Achievements"]:
                continue
            # Determine the wrapper key from existing entries
            existing = next(iter(title["Achievements"].values()), None)
            wrapper_key = "Replacement"
            if isinstance(existing, dict):
                keys = [k for k in existing.keys()
                        if isinstance(existing[k], dict)
                        and existing[k].get("Target") == placeholder]
                if keys:
                    wrapper_key = keys[0]
            title["Achievements"][aid] = {
                wrapper_key: {
                    "ReplacementType": "Replace",
                    "Target": placeholder,
                    "Replacement": n,
                }
            }
            merged += 1
    if merged:
        # Backup before save
        bak = DATA_JSON.with_suffix(".json.bak.before-discovery")
        if not bak.exists():
            import shutil
            shutil.copy(DATA_JSON, bak)
        with open(DATA_JSON, "w", encoding="utf-8") as f:
            json.dump(data, f, separators=(",", ":"), ensure_ascii=False)
        print(f"\nMerged {merged} new ach templates into Data.json")
    else:
        print("\nNothing to merge.")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--title", help="single title id to discover")
    p.add_argument("--all-simple", action="store_true",
                   help="discover all partial titles with simple-replacement templates")
    p.add_argument("--max-n", type=int, default=100, help="max progression value to try")
    p.add_argument("--delay", type=float, default=1.5,
                   help="seconds to wait between POST and re-scan")
    p.add_argument("--merge-only", action="store_true",
                   help="just merge discovered/*.json into Data.json without running discovery")
    p.add_argument("--xuid", help="override XUID (skip profile endpoint and JWT decode)")
    args = p.parse_args()

    if args.merge_only:
        merge_into_bundle()
        return

    DISCOVERED_DIR.mkdir(parents=True, exist_ok=True)
    xauth, events_xauth = load_auth()
    print(f"Loaded auth: events_xauth set={bool(events_xauth)}")
    if args.xuid:
        xuid = args.xuid
        print(f"XUID (override): {xuid}")
    else:
        xuid = get_xuid(xauth)
        print(f"XUID: {xuid}")

    targets = []
    if args.title:
        targets = [args.title]
    elif args.all_simple:
        # Pick simple-template titles from bundle
        with open(DATA_JSON, encoding="utf-8") as f:
            data = json.load(f)
        for tid, td in data.items():
            if not isinstance(td, dict):
                continue
            achs = td.get("Achievements")
            if not isinstance(achs, dict) or td.get("FullySupported"):
                continue
            sample = next(iter(achs.values()), None)
            if isinstance(sample, dict) and len(sample) == 1:
                # Single-rule template
                rule = next(iter(sample.values()))
                if isinstance(rule, dict) and rule.get("ReplacementType") == "Replace":
                    targets.append(tid)
        print(f"Will discover {len(targets)} simple-template titles: {targets}")
    else:
        sys.exit("specify --title <id> or --all-simple")

    summary = []
    for tid in targets:
        print(f"\n{'='*60}")
        print(f"Title: {tid}")
        print(f"{'='*60}")
        try:
            r = discover_title(tid, xauth, events_xauth, xuid,
                               max_n=args.max_n, delay=args.delay)
            summary.append(r)
        except Exception as e:
            print(f"FAIL: {e}")
            summary.append({"title_id": tid, "error": str(e)})

    # Merge & summary
    print(f"\n{'='*60}")
    print("DISCOVERY SUMMARY")
    print(f"{'='*60}")
    for s in summary:
        if "error" in s and "discovered_count" not in s:
            print(f"  {s['title_id']:>15s}: ERROR {s['error']}")
        else:
            print(f"  {s['title_id']:>15s}: existing={s.get('existing_count','?')}, "
                  f"discovered={s.get('discovered_count','?')}")

    print()
    merge_into_bundle()


if __name__ == "__main__":
    main()
