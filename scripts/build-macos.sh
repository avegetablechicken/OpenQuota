#!/bin/bash
# Build the host-architecture macOS app or DMG using the installed dependencies.
set -euo pipefail

cd "$(dirname "$0")/.."
mode=app
checks=false
for argument in "$@"; do
  case "$argument" in
    app|dmg) mode="$argument" ;;
    --check) checks=true ;;
    -h|--help)
      echo 'Usage: bash scripts/build-macos.sh [app|dmg] [--check]'
      echo 'Defaults to app. --check runs the full repository checks before building.'
      exit 0 ;;
    *) echo "Unknown argument: $argument" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'This script requires macOS.' >&2
  exit 1
fi
export PATH="/tmp/openquota-pnpm-shim:/opt/homebrew/bin:/usr/bin:/bin:$PATH"
# A linker-only signature breaks native notification authorization. Keep an
# explicitly configured Developer ID identity; otherwise sign the entire bundle.
export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
if [[ ! -x node_modules/.bin/tauri || ! -x node_modules/.bin/vite ]]; then
  echo 'Missing node_modules dependencies. Install the project dependencies first.' >&2
  exit 1
fi
if [[ -n "${CARGO_TARGET_DIR:-}" || -n "${CARGO_BUILD_TARGET:-}" ]]; then
  echo 'Unset CARGO_TARGET_DIR and CARGO_BUILD_TARGET for this host-architecture build.' >&2
  exit 1
fi

if "$checks"; then
  cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
  cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
  cargo test --manifest-path src-tauri/Cargo.toml --all-targets
  node_modules/.bin/prettier --check '.github/**/*.{yml,yaml}' '*.{json,md,js,ts}' scripts/verify/*.js 'src/**/*.{ts,svelte,css}' src-tauri/tauri.conf.json src-tauri/capabilities/default.json
  node_modules/.bin/svelte-check --tsconfig ./tsconfig.app.json
  node_modules/.bin/eslint eslint.config.js vite.config.ts scripts/verify/*.js src
  node_modules/.bin/vitest run
  git diff --check
fi

version="$(node -p 'require("./package.json").version')"
identifier="$(node -p 'require("./src-tauri/tauri.conf.json").identifier')"
case "$(uname -m)" in
  arm64) architecture=aarch64 ;;
  x86_64) architecture=x64 ;;
  *) echo 'Unsupported macOS architecture.' >&2; exit 1 ;;
esac
bundle_dir="$PWD/src-tauri/target/release/bundle"
app_path="$bundle_dir/macos/OpenQuota.app"
dmg_path="$bundle_dir/dmg/OpenQuota_${version}_${architecture}.dmg"
build_log="$(mktemp -t openquota-build)"
trap 'rm -f "$build_log"' EXIT
build_arguments=(--bundles "$mode")
if [[ "$mode" == dmg ]]; then
  # Tauri hides the Finder error code without verbose output, preventing the
  # fallback from distinguishing permission errors from real packaging failures.
  build_arguments=(--verbose "${build_arguments[@]}")
fi

if PATH=/tmp/openquota-pnpm-shim:/opt/homebrew/bin:/usr/bin:/bin \
  node_modules/.bin/tauri build \
    "${build_arguments[@]}" \
    --ignore-version-mismatches \
    --config '{"build":{"beforeBuildCommand":"node_modules/.bin/vite build"}}' \
    2>&1 | tee "$build_log"; then
  :
elif [[ "$mode" == dmg ]] && grep -q -- '-1743' "$build_log"; then
  echo 'Finder Automation permission is unavailable; creating the DMG without Finder layout.'
  # Detach only this build's temporary images, never an unrelated mounted volume.
  python3 - "$dmg_path" <<'PY'
import pathlib
import plistlib
import re
import subprocess
import sys

target = pathlib.Path(sys.argv[1])
images = plistlib.loads(subprocess.check_output(['hdiutil', 'info', '-plist']))
for image in images.get('images', []):
    path = pathlib.Path(image.get('image-path', ''))
    if path.parent != target.parent or not re.fullmatch(r'rw\.\d+\.' + re.escape(target.name), path.name):
        continue
    devices = [entry.get('dev-entry', '') for entry in image.get('system-entities', [])]
    device = next((value for value in devices if re.fullmatch(r'/dev/disk\d+', value)), None)
    if device is None:
        raise SystemExit('Could not identify the temporary image device; inspect hdiutil info.')
    subprocess.run(['hdiutil', 'detach', device], check=True)
PY
  (
    cd "$bundle_dir/macos"
    ../dmg/bundle_dmg.sh \
      --skip-jenkins \
      --volname OpenQuota \
      --icon OpenQuota.app 180 170 \
      --app-drop-link 480 170 \
      --window-size 660 400 \
      --hide-extension OpenQuota.app \
      --volicon ../dmg/icon.icns \
      "$dmg_path" \
      OpenQuota.app
  )
else
  echo 'macOS build failed; see the build output above.' >&2
  exit 1
fi

codesign --verify --deep --strict "$app_path"
signature="$(codesign -dv "$app_path" 2>&1)"
printf '%s\n' "$signature"
if ! grep -Fxq "Identifier=$identifier" <<< "$signature" ||
  ! grep -q '^Info.plist entries=' <<< "$signature"; then
  echo 'Invalid bundle signature: the identifier must match the app and Info.plist must be bound.' >&2
  exit 1
fi

if [[ "$mode" == dmg ]]; then
  hdiutil verify "$dmg_path"
  python3 - "$dmg_path" <<'PY'
import pathlib
import plistlib
import re
import subprocess
import sys

mounted = plistlib.loads(subprocess.check_output([
    'hdiutil', 'attach', '-readonly', '-nobrowse', '-plist', sys.argv[1]
]))
entries = mounted['system-entities']
device = next(entry['dev-entry'] for entry in entries if re.fullmatch(r'/dev/disk\d+', entry.get('dev-entry', '')))
try:
    volume = pathlib.Path(next(entry['mount-point'] for entry in entries if 'mount-point' in entry))
    app = volume / 'OpenQuota.app'
    applications = volume / 'Applications'
    if not app.is_dir() or not applications.is_symlink() or applications.readlink() != pathlib.Path('/Applications'):
        raise SystemExit('DMG must contain OpenQuota.app and Applications -> /Applications.')
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
    subprocess.run(['ls', '-la', str(volume)], check=True)
finally:
    subprocess.run(['hdiutil', 'detach', device], check=True)
PY
  shasum -a 256 "$dmg_path"
  printf '\nDMG: %s\n' "$dmg_path"
fi
printf '\nApp: %s\n' "$app_path"
