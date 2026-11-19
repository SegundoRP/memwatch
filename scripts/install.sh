#!/bin/sh
# Installs (or updates) memwatch as a LaunchAgent: it runs `memwatch watch` from
# the moment you log in, and launchd restarts it if it dies (even with kill -9).
# Details and pitfalls: docs/notes/m5-launchd.md.
set -eu

LABEL="com.segundorp.memwatch"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/memwatch"
LOG_DIR="$HOME/Library/Logs"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

cd "$(dirname "$0")/.."
cargo build --release

mkdir -p "$BIN_DIR" "$LOG_DIR" "$(dirname "$PLIST")"

# Never `cp` over the running binary (macOS caches the signature per file and
# may kill it): copy to a temporary file in the SAME folder and rename it,
# which is atomic.
cp target/release/memwatch "$BIN_DIR/.memwatch.new"
mv -f "$BIN_DIR/.memwatch.new" "$BIN"

cat > "$PLIST.new" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN</string>
        <string>watch</string>
        <string>--log</string>
        <string>$LOG_DIR/memwatch.log</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>$LOG_DIR/memwatch.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>$LOG_DIR/memwatch.stderr.log</string>
</dict>
</plist>
PLIST
plutil -lint "$PLIST.new" >/dev/null
chmod 644 "$PLIST.new"
mv -f "$PLIST.new" "$PLIST"

# If it was already loaded, unload it first so it picks up the new plist.
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    launchctl bootout "$DOMAIN/$LABEL" || true
    while launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; do sleep 0.2; done
fi
launchctl bootstrap "$DOMAIN" "$PLIST"

echo "memwatch installed at $BIN"
echo "log:    $LOG_DIR/memwatch.log"
echo "status: launchctl print $DOMAIN/$LABEL"
