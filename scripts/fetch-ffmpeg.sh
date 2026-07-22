#!/usr/bin/env sh
set -eu

version=8.1.2
expected_sha256=464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c
url="https://ffmpeg.org/releases/ffmpeg-$version.tar.xz"
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository=$(CDPATH= cd -- "$script_dir/.." && pwd)
destination="$repository/vendor/ffmpeg"
patch_file="$repository/patches/ffmpeg-8.1.2-vaapi-configure.patch"
force=false

usage() {
    cat <<'EOF'
Usage: scripts/fetch-ffmpeg.sh [--force]

Download and verify the pinned FFmpeg source archive, then apply the tracked
Chatt VAAPI configure patch. Existing differing source is preserved unless
--force is supplied.
EOF
}

case "${1:-}" in
    "")
        ;;
    --force)
        force=true
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

for program in curl sha256sum tar diff mktemp patch; do
    if ! command -v "$program" >/dev/null 2>&1; then
        echo "Required program is unavailable: $program" >&2
        exit 1
    fi
done

temporary=$(mktemp -d "${TMPDIR:-/tmp}/chatt-gui-ffmpeg.XXXXXX")
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

archive="$temporary/ffmpeg-$version.tar.xz"
extracted="$temporary/extracted"
mkdir -p -- "$extracted"

echo "Downloading FFmpeg $version..."
curl -fsSL "$url" -o "$archive"
actual_sha256=$(sha256sum "$archive")
actual_sha256=${actual_sha256%% *}
if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "FFmpeg archive checksum mismatch." >&2
    echo "Expected: $expected_sha256" >&2
    echo "Actual:   $actual_sha256" >&2
    exit 1
fi

tar -xJf "$archive" --strip-components=1 -C "$extracted"
if [ ! -x "$extracted/configure" ] || [ ! -f "$extracted/VERSION" ]; then
    echo "FFmpeg archive did not contain the expected source tree." >&2
    exit 1
fi
if ! patch --batch --forward -d "$extracted" -p1 < "$patch_file"; then
    echo "Could not apply the tracked FFmpeg configure patch." >&2
    exit 1
fi

if [ -e "$destination" ]; then
    if diff -qr -- "$destination" "$extracted" >/dev/null; then
        echo "FFmpeg $version source is already present and verified."
        exit 0
    fi
    if [ "$force" != true ]; then
        echo "Existing FFmpeg source differs from the pinned archive: $destination" >&2
        echo "Rerun with --force to replace it." >&2
        exit 1
    fi

    previous="$temporary/previous"
    mv -- "$destination" "$previous"
    if ! mv -- "$extracted" "$destination"; then
        mv -- "$previous" "$destination"
        echo "Could not install the verified FFmpeg source." >&2
        exit 1
    fi
    rm -rf -- "$previous"
else
    mkdir -p -- "$repository/vendor"
    mv -- "$extracted" "$destination"
fi

echo "Installed verified FFmpeg $version source at $destination"
