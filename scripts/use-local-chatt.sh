#!/usr/bin/env sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository=$(CDPATH= cd -- "$script_dir/.." && pwd)
config="$repository/.cargo/local.toml"

usage() {
    cat <<'EOF'
Usage: scripts/use-local-chatt.sh [CHATT_CHECKOUT]
       scripts/use-local-chatt.sh --pinned

Use a local Chatt checkout for this repository's Chatt crate dependencies.
CHATT_CHECKOUT defaults to /code/chatt. Pass --pinned to restore the pinned Git
dependencies from Cargo.toml.
EOF
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    --pinned)
        rm -f -- "$config"
        echo "Using the pinned Chatt Git revision."
        exit 0
        ;;
esac

checkout=${1:-/code/chatt}
if ! checkout=$(CDPATH= cd -- "$checkout" 2>/dev/null && pwd); then
    echo "Chatt checkout does not exist: $checkout" >&2
    exit 1
fi

for manifest in \
    "$checkout/crates/local-rpc/Cargo.toml" \
    "$checkout/crates/message-format/Cargo.toml"
do
    if [ ! -f "$manifest" ]; then
        echo "Required Chatt crate is missing: $manifest" >&2
        exit 1
    fi
done

case "$checkout" in
    *"'"*|*"
"*)
        echo "Chatt checkout path contains a character unsupported by this helper: $checkout" >&2
        exit 1
        ;;
esac

mkdir -p -- "$repository/.cargo"
cat > "$config" <<EOF
[patch."https://github.com/chatt-im/chatt.git"]
chatt-local-rpc = { path = '$checkout/crates/local-rpc' }
chatt-message-format = { path = '$checkout/crates/message-format' }
EOF

echo "Using local Chatt checkout: $checkout"
