#!/usr/bin/env sh
set -eu

vulkan_headers_commit=450bd2232225d6c7728a4108055ac2e37cef6475
vulkan_headers_sha256=26df9841c30806a994e2fdf42f7c87bcb1ced9db9a06033469123939fb3fa075
jinja_commit=15206881c006c79667fe5154fe80c01c65410679
jinja_sha256=b88a20dcc2e34072fcf4159325bc6c34cd4b29a81a8b83d15d2f28ba561da296
markupsafe_commit=297fc8e356e6836a62087949245d09a28e9f1b13
markupsafe_sha256=da7c010c9c81a66ac73036558c1fcb6212b50482f43211cd1254035b94f82414

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository=$(CDPATH= cd -- "$script_dir/.." && pwd)
thirdparty="$repository/vendor/libplacebo/3rdparty"
force=false

usage() {
    cat <<'EOF'
Usage: scripts/fetch-libplacebo-deps.sh [--force]

Download and verify the pinned Vulkan-Headers, Jinja, and MarkupSafe snapshots
required by the vendored libplacebo build. Existing differing sources are
preserved unless --force is supplied.
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

for program in curl sha256sum tar diff mktemp; do
    if ! command -v "$program" >/dev/null 2>&1; then
        echo "Required program is unavailable: $program" >&2
        exit 1
    fi
done

temporary=$(mktemp -d "${TMPDIR:-/tmp}/chatt-gui-libplacebo.XXXXXX")
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

install_snapshot() {
    name=$1
    url=$2
    expected_sha256=$3
    destination=$4
    archive="$temporary/$name.tar.gz"
    extracted="$temporary/$name"

    echo "Downloading $name..."
    curl -fsSL "$url" -o "$archive"
    actual_sha256=$(sha256sum "$archive")
    actual_sha256=${actual_sha256%% *}
    if [ "$actual_sha256" != "$expected_sha256" ]; then
        echo "$name archive checksum mismatch." >&2
        echo "Expected: $expected_sha256" >&2
        echo "Actual:   $actual_sha256" >&2
        exit 1
    fi

    mkdir -p -- "$extracted"
    tar -xzf "$archive" --strip-components=1 -C "$extracted"
    if [ ! -f "$extracted/LICENSE.txt" ] &&
        [ ! -f "$extracted/LICENSE.md" ]; then
        echo "$name archive did not contain the expected source tree." >&2
        exit 1
    fi

    if [ -e "$destination" ]; then
        if diff -qr --exclude=__pycache__ --exclude='*.pyc' \
            -- "$destination" "$extracted" >/dev/null; then
            echo "$name is already present and verified."
            return
        fi
        if [ "$force" != true ]; then
            echo "Existing $name source differs: $destination" >&2
            echo "Rerun with --force to replace it." >&2
            exit 1
        fi

        previous="$temporary/$name.previous"
        mv -- "$destination" "$previous"
        if ! mv -- "$extracted" "$destination"; then
            mv -- "$previous" "$destination"
            echo "Could not install $name." >&2
            exit 1
        fi
        rm -rf -- "$previous"
    else
        mkdir -p -- "$thirdparty"
        mv -- "$extracted" "$destination"
    fi

    echo "Installed $name at $destination"
}

install_snapshot \
    Vulkan-Headers \
    "https://github.com/KhronosGroup/Vulkan-Headers/archive/$vulkan_headers_commit.tar.gz" \
    "$vulkan_headers_sha256" \
    "$thirdparty/Vulkan-Headers"
install_snapshot \
    jinja \
    "https://github.com/pallets/jinja/archive/$jinja_commit.tar.gz" \
    "$jinja_sha256" \
    "$thirdparty/jinja"
install_snapshot \
    markupsafe \
    "https://github.com/pallets/markupsafe/archive/$markupsafe_commit.tar.gz" \
    "$markupsafe_sha256" \
    "$thirdparty/markupsafe"
