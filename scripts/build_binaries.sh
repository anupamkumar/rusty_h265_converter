#!/usr/bin/env bash
set -euo pipefail

# Build release (and optional standalone unit-test) binaries for rusty_h265_converter.
#
# Usage:
#   ./scripts/build_binaries.sh <platform> <arch> [--with-tests]
#   ./scripts/build_binaries.sh all [--with-tests]
#
# Supported <platform> <arch>:
#   macos   arm64   -> aarch64-apple-darwin
#   macos   x86_64  -> x86_64-apple-darwin
#   linux   x86_64  -> x86_64-unknown-linux-gnu.2.31
#   linux   arm64   -> aarch64-unknown-linux-gnu.2.31
#   windows x86_64  -> x86_64-pc-windows-gnu
#   windows arm64   -> aarch64-pc-windows-gnullvm

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DIST_DIR="${REPO_ROOT}/dist"

export PATH="/opt/homebrew/bin:${HOME}/.cargo/bin:${PATH}"

usage() {
    cat <<EOF
Usage:
  $0 <platform> <arch> [--with-tests]
  $0 all [--with-tests]

Platforms & Architectures:
  macos   arm64 | x86_64
  linux   x86_64 | arm64
  windows x86_64 | arm64

Options:
  --with-tests    Also build standalone unit-test runner binary into dist/
EOF
    exit 1
}

if [[ $# -lt 1 ]]; then
    usage
fi

WITH_TESTS=0
ARGS=()
for arg in "$@"; do
    case "${arg}" in
        --with-tests)
            WITH_TESTS=1
            ;;
        -h|--help)
            usage
            ;;
        *)
            ARGS+=("${arg}")
            ;;
    esac
done

if [[ ${#ARGS[@]} -lt 1 ]]; then
    usage
fi

resolve_target() {
    local platform="$1"
    local arch="$2"
    case "${platform}:${arch}" in
        macos:arm64|macos:aarch64)
            echo "aarch64-apple-darwin|aarch64-apple-darwin||native"
            ;;
        macos:x86_64|macos:x64|macos:amd64)
            echo "x86_64-apple-darwin|x86_64-apple-darwin||native"
            ;;
        linux:x86_64|linux:x64|linux:amd64)
            echo "x86_64-unknown-linux-gnu|x86_64-unknown-linux-gnu.2.31||zigbuild"
            ;;
        linux:arm64|linux:aarch64)
            echo "aarch64-unknown-linux-gnu|aarch64-unknown-linux-gnu.2.31||zigbuild"
            ;;
        windows:x86_64|windows:x64|windows:amd64)
            echo "x86_64-pc-windows-gnu|x86_64-pc-windows-gnu|.exe|zigbuild"
            ;;
        windows:arm64|windows:aarch64)
            echo "aarch64-pc-windows-gnullvm|aarch64-pc-windows-gnullvm|.exe|zigbuild"
            ;;
        *)
            echo "ERROR: Unsupported platform/arch combination: ${platform} ${arch}" >&2
            usage
            ;;
    esac
}

normalize_arch() {
    case "$1" in
        aarch64|arm64) echo "arm64" ;;
        x86_64|x64|amd64) echo "x86_64" ;;
        *) echo "$1" ;;
    esac
}

build_one() {
    local platform="$1"
    local arch
    arch="$(normalize_arch "$2")"

    local spec
    spec="$(resolve_target "${platform}" "${arch}")"
    IFS='|' read -r rust_triple build_target ext builder <<< "${spec}"

    if [[ "${builder}" == "zigbuild" ]]; then
        if ! command -v cargo-zigbuild >/dev/null 2>&1; then
            echo "ERROR: cargo-zigbuild not found in PATH. Install via: cargo install --locked cargo-zigbuild" >&2
            exit 1
        fi
        if ! command -v zig >/dev/null 2>&1; then
            echo "ERROR: zig not found in PATH. Install via: brew install zig" >&2
            exit 1
        fi
    fi

    echo "================================================================"
    echo "[build_binaries] Building release: ${platform}/${arch} (target=${build_target}, builder=${builder})"
    echo "================================================================"

    mkdir -p "${DIST_DIR}"
    cd "${REPO_ROOT}"

    if [[ "${builder}" == "native" ]]; then
        cargo build --release --target "${build_target}"
    else
        cargo zigbuild --release --target "${build_target}"
    fi

    local src_bin="${REPO_ROOT}/target/${rust_triple}/release/rusty_h265_converter${ext}"
    local dst_bin="${DIST_DIR}/rusty_h265_converter-${platform}-${arch}${ext}"
    cp -f "${src_bin}" "${dst_bin}"
    if [[ -z "${ext}" ]]; then
        chmod +x "${dst_bin}"
    fi
    echo "[build_binaries] -> ${dst_bin}"

    if [[ "${WITH_TESTS}" -eq 1 ]]; then
        echo "[build_binaries] Building standalone unit-test binary: ${platform}/${arch}..."
        local json_out
        if [[ "${builder}" == "native" ]]; then
            json_out="$(cargo test --no-run --target "${build_target}" --message-format=json)"
        else
            json_out="$(cargo zigbuild --tests --target "${build_target}" --message-format=json)"
        fi

        local test_bin
        test_bin="$(printf '%s\n' "${json_out}" | python3 -c '
import json, sys
found = ""
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("reason") == "compiler-artifact" and msg.get("profile", {}).get("test") and msg.get("executable"):
        if msg.get("target", {}).get("name") == "rusty_h265_converter":
            found = msg["executable"]
print(found)
')"
        if [[ -z "${test_bin}" || ! -f "${test_bin}" ]]; then
            echo "ERROR: Could not locate compiled unit-test binary for ${platform}/${arch}" >&2
            exit 1
        fi

        local dst_test="${DIST_DIR}/rusty_h265_converter-tests-${platform}-${arch}${ext}"
        cp -f "${test_bin}" "${dst_test}"
        if [[ -z "${ext}" ]]; then
            chmod +x "${dst_test}"
        fi
        echo "[build_binaries] -> ${dst_test}"
    fi
}

if [[ "${ARGS[0]}" == "all" ]]; then
    build_one macos   arm64
    build_one macos   x86_64
    build_one linux   x86_64
    build_one linux   arm64
    build_one windows x86_64
    build_one windows arm64
else
    if [[ ${#ARGS[@]} -lt 2 ]]; then
        usage
    fi
    build_one "${ARGS[0]}" "${ARGS[1]}"
fi

# Update SHA-256 checksums for all artifacts in dist/
if ls "${DIST_DIR}"/rusty_h265_converter-* >/dev/null 2>&1; then
    (
        cd "${DIST_DIR}"
        shasum -a 256 rusty_h265_converter-* > SHA256SUMS.txt
    )
    echo "[build_binaries] Updated ${DIST_DIR}/SHA256SUMS.txt"
fi
