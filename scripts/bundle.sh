#!/bin/sh
# Build Warren.app in target/release, with the app icon.
#
#   scripts/bundle.sh [--install]
#
# --install also copies it to /Applications.
set -eu
cd "$(dirname "$0")/.."

cargo build --release

app=target/release/Warren.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/warren "$app/Contents/MacOS/warren"
cp assets/icon/Warren.icns "$app/Contents/Resources/Warren.icns"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Warren</string>
  <key>CFBundleDisplayName</key><string>Warren</string>
  <key>CFBundleIdentifier</key><string>com.austinsimpson.warren</string>
  <key>CFBundleExecutable</key><string>warren</string>
  <key>CFBundleIconFile</key><string>Warren</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc sign so macOS will launch it.
codesign --force --deep --sign - "$app"

if [ "${1:-}" = "--install" ]; then
  rm -rf /Applications/Warren.app
  cp -R "$app" /Applications/
  echo "Installed /Applications/Warren.app"
else
  echo "Built $app"
fi
