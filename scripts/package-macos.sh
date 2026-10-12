#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "This packaging script must run on macOS." >&2
    exit 1
fi

for dependency in cargo ffmpeg ffprobe ffplay; do
    if ! command -v "$dependency" >/dev/null 2>&1; then
        echo "Missing dependency: $dependency (install it and retry)." >&2
        exit 1
    fi
done

version=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
if [ -z "$version" ]; then
    echo "Could not read the package version from Cargo.toml." >&2
    exit 1
fi

output_dir=${VIDEOPRO_PACKAGE_DIR:-dist}
app_path="$output_dir/VideoPro.app"
bundle_id=${VIDEOPRO_BUNDLE_ID:-dev.videopro.VideoPro}
case "$bundle_id" in
    ''|*[!A-Za-z0-9.-]*)
        echo "VIDEOPRO_BUNDLE_ID may contain only letters, digits, dots, and hyphens." >&2
        exit 1
        ;;
esac

if [ -e "$app_path" ]; then
    echo "Refusing to overwrite existing package: $app_path" >&2
    echo "Move the existing bundle aside or choose VIDEOPRO_PACKAGE_DIR." >&2
    exit 1
fi

cargo build --release --locked
stage=$(mktemp -d "${TMPDIR:-/tmp}/videopro-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
staged_app="$stage/VideoPro.app"
mkdir -p "$staged_app/Contents/MacOS" "$staged_app/Contents/Resources"
cp target/release/videopro "$staged_app/Contents/MacOS/videopro"
chmod 755 "$staged_app/Contents/MacOS/videopro"

cat > "$staged_app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>videopro</string>
    <key>CFBundleIdentifier</key><string>$bundle_id</string>
    <key>CFBundleName</key><string>VideoPro</string>
    <key>CFBundleDisplayName</key><string>VideoPro</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

mkdir -p "$output_dir"
mv "$staged_app" "$app_path"
echo "Created $app_path"
echo "FFmpeg, ffprobe, and ffplay remain external runtime dependencies; this bundle is not signed or notarized."
