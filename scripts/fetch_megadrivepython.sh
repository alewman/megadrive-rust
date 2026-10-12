#!/usr/bin/env bash
# Check out the megadrive-python reference at the frozen pin and install it, with exactly
# the four cores its pyproject pins, into a PyPy virtual environment, so the lockstep
# gate runs against that board.
# Usage: scripts/fetch_megadrivepython.sh [destination]
# MEGADRIVE_PYTHON_SOURCE overrides where it is cloned from (a local clone will do);
# PYTHON names the interpreter (PyPy by default, which runs the reference fastest).
set -euo pipefail
TAG=${MEGADRIVE_PYTHON_TAG:-megadrive-rust-pin-1}
COMMIT=${MEGADRIVE_PYTHON_COMMIT:-d7882e1}
SOURCE=${MEGADRIVE_PYTHON_SOURCE:-git@github.com:alewman/megadrive-python.git}
DEST=${1:-"$(dirname "$0")/../external/megadrive-python"}
PYTHON=${PYTHON:-$(command -v pypy3.11 || command -v python3)}
if [ ! -d "$DEST/.git" ]; then
    git clone -q "$SOURCE" "$DEST"
fi
git -C "$DEST" fetch -q --tags origin
git -C "$DEST" checkout -q --detach "$TAG"
HEAD=$(git -C "$DEST" rev-parse HEAD)
case "$HEAD" in
    "$COMMIT"*) ;;
    *) echo "tag $TAG is at $HEAD, not $COMMIT" >&2; exit 1 ;;
esac
if [ ! -x "$DEST/.venv/bin/python" ]; then
    "$PYTHON" -m venv "$DEST/.venv"
fi
"$DEST/.venv/bin/python" -m pip install -q -e "$DEST"
"$DEST/.venv/bin/python" -m pip freeze | grep -E "python @ git" || true
echo "megadrive-python $TAG ($(git -C "$DEST" rev-parse --short HEAD)) installed in $DEST/.venv"
