#!/bin/sh
# Print an Ocean Buddy pairing QR code for the iPhone app to scan.
#
# Usage:
#   pairing-qr.sh <daemon-url> [session-id]
#   pairing-qr.sh https://ocean.example.com
#   pairing-qr.sh http://risings-mac-mini.local:4780 00000000-0000-4000-8000-000000000001
#
# The payload carries the daemon address and optional session ID only — never
# provider keys or minted credentials. Rendering uses `qrencode` when
# installed (brew install qrencode); otherwise the payload string is printed
# for manual entry or an external generator.
set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <daemon-url> [session-id]" >&2
    exit 2
fi

DAEMON_URL=$1
SESSION_ID=${2:-}

PAYLOAD=$(python3 - "$DAEMON_URL" "$SESSION_ID" <<'PY'
import sys
import urllib.parse
import uuid

daemon = sys.argv[1]
session = sys.argv[2] if len(sys.argv) > 2 else ""
parsed = urllib.parse.urlsplit(daemon)
if parsed.scheme not in ("http", "https") or not parsed.netloc:
    sys.exit(f"error: not an http(s) daemon URL: {daemon}")
if parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ("", "/"):
    sys.exit("error: daemon URL must be scheme://host[:port] with no path, query, or credentials")

query = {"v": "1", "daemon": daemon}
if session:
    try:
        session = str(uuid.UUID(session))
    except ValueError:
        sys.exit("error: session id must be an Ocean UUID")
    query["session"] = session
print("ocean-buddy://pair?" + urllib.parse.urlencode(query))
PY
)

echo "payload: $PAYLOAD"
echo

if command -v qrencode >/dev/null 2>&1; then
    qrencode -t ANSIUTF8 "$PAYLOAD"
else
    echo "qrencode is not installed; install it to render the QR here:"
    echo "  brew install qrencode"
    echo "Then rerun, or paste the payload into any QR generator."
fi
