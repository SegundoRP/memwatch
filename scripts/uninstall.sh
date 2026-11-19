#!/bin/sh
# Uninstalls the memwatch LaunchAgent. `bootout` alone is not enough: at the next
# login launchd would load the plist again, so it is deleted too.
set -eu

LABEL="com.segundorp.memwatch"
DOMAIN="gui/$(id -u)"

launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist" "$HOME/.local/bin/memwatch"
echo "memwatch uninstalled. Logs remain in ~/Library/Logs/memwatch*.log"
