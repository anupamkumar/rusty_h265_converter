#!/usr/bin/env bash
set -euo pipefail

# Build, deploy (if remote), and verify rusty_h265_converter release + base + platform unit tests.
#
# Usage:
#   ./scripts/verify.sh macos arm64
#   ./scripts/verify.sh linux arm64 <user@host> [ssh_port]
#   ./scripts/verify.sh windows x86_64 <user@host> [ssh_port]
#
# Optional environment variables (used when <user@host> is omitted):
#   VERIFY_LINUX_ARM64_HOST      e.g. pi@192.168.68.150
#   VERIFY_LINUX_X86_64_HOST     e.g. dev@192.168.68.126
#   VERIFY_WINDOWS_X86_64_HOST   e.g. anu@192.168.68.120
#   VERIFY_WINDOWS_ARM64_HOST    e.g. user@192.168.68.121

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DIST_DIR="${REPO_ROOT}/dist"

usage() {
    cat <<EOF
Usage:
  $0 macos arm64
  $0 linux <arm64|x86_64> [user@host] [ssh_port]
  $0 windows <x86_64|arm64> [user@host] [ssh_port]

Runs:
  1. Builds release binary + standalone unit-test binary via scripts/build_binaries.sh --with-tests
  2. Copies binary, test binary, config.yaml, logging.conf, and metrics agent to target (if remote)
  3. Runs release binary --help smoke check
  4. Runs base unit tests (tests::)
  5. Runs platform-specific unit tests (windows_tests:: or linux_tests::)
EOF
    exit 1
}

if [[ $# -lt 2 ]]; then
    usage
fi

PLATFORM="$1"
RAW_ARCH="$2"
REMOTE_HOST="${3:-}"
SSH_PORT="${4:-22}"

case "${RAW_ARCH}" in
    aarch64|arm64) ARCH="arm64" ;;
    x86_64|x64|amd64) ARCH="x86_64" ;;
    *) echo "ERROR: Unsupported architecture: ${RAW_ARCH}" >&2; usage ;;
esac

# Step 1: Ensure both release and test binaries are freshly built
"${SCRIPT_DIR}/build_binaries.sh" "${PLATFORM}" "${ARCH}" --with-tests

# Step 2: Execute verification on target platform
if [[ "${PLATFORM}" == "macos" && "${ARCH}" == "arm64" ]]; then
    REL_BIN="${DIST_DIR}/rusty_h265_converter-macos-arm64"
    TEST_BIN="${DIST_DIR}/rusty_h265_converter-tests-macos-arm64"

    echo "================================================================"
    echo "[verify] Local macOS ARM64 Verification"
    echo "================================================================"
    echo "[verify] 1/2 Release binary --help smoke check..."
    "${REL_BIN}" --help >/dev/null
    echo "[verify]     OK"

    echo "[verify] 2/2 Running base unit tests (tests::)..."
    RUSTY_H265_TEST_ROOT="${REPO_ROOT}" "${TEST_BIN}" tests::
    echo "[verify] PASS: macos/arm64 verification succeeded."
    exit 0
fi

if [[ "${PLATFORM}" == "linux" ]]; then
    if [[ -z "${REMOTE_HOST}" ]]; then
        if [[ "${ARCH}" == "arm64" ]]; then
            REMOTE_HOST="${VERIFY_LINUX_ARM64_HOST:-}"
        else
            REMOTE_HOST="${VERIFY_LINUX_X86_64_HOST:-}"
        fi
    fi
    if [[ -z "${REMOTE_HOST}" ]]; then
        echo "ERROR: Remote Linux host required. Pass <user@host> or set VERIFY_LINUX_ARM64_HOST / VERIFY_LINUX_X86_64_HOST." >&2
        exit 1
    fi

    REL_BIN="${DIST_DIR}/rusty_h265_converter-linux-${ARCH}"
    TEST_BIN="${DIST_DIR}/rusty_h265_converter-tests-linux-${ARCH}"
    REMOTE_DIR="/tmp/rusty_h265_verify_$$"

    cleanup_linux() {
        ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" "rm -rf '${REMOTE_DIR}'" >/dev/null 2>&1 || true
    }
    trap cleanup_linux EXIT

    echo "================================================================"
    echo "[verify] Remote Linux (${ARCH}) Verification on ${REMOTE_HOST}:${SSH_PORT}"
    echo "================================================================"

    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" "mkdir -p '${REMOTE_DIR}/scripts'"

    echo "[verify] Deploying release binary, test binary, and configs via SCP..."
    scp -q -o BatchMode=yes -o ConnectTimeout=10 -P "${SSH_PORT}" \
        "${REL_BIN}" \
        "${TEST_BIN}" \
        "${REPO_ROOT}/config.yaml" \
        "${REPO_ROOT}/logging.conf" \
        "${REMOTE_HOST}:${REMOTE_DIR}/"
    scp -q -o BatchMode=yes -o ConnectTimeout=10 -P "${SSH_PORT}" \
        "${REPO_ROOT}/scripts/rusty_h265_metrics_agent.py" \
        "${REMOTE_HOST}:${REMOTE_DIR}/scripts/"

    echo "[verify] 1/3 Release binary --help smoke check..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "chmod +x '${REMOTE_DIR}/rusty_h265_converter-linux-${ARCH}' '${REMOTE_DIR}/rusty_h265_converter-tests-linux-${ARCH}' && '${REMOTE_DIR}/rusty_h265_converter-linux-${ARCH}' --help >/dev/null"
    echo "[verify]     OK"

    echo "[verify] 2/3 Running base unit tests (tests::) on ${REMOTE_HOST}..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "cd '${REMOTE_DIR}' && RUSTY_H265_TEST_ROOT='${REMOTE_DIR}' './rusty_h265_converter-tests-linux-${ARCH}' tests::"

    echo "[verify] 3/3 Running Linux platform unit tests (linux_tests::) on ${REMOTE_HOST}..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "cd '${REMOTE_DIR}' && RUSTY_H265_TEST_ROOT='${REMOTE_DIR}' './rusty_h265_converter-tests-linux-${ARCH}' linux_tests::"

    echo "[verify] PASS: linux/${ARCH} verification on ${REMOTE_HOST} succeeded."
    exit 0
fi

if [[ "${PLATFORM}" == "windows" ]]; then
    if [[ -z "${REMOTE_HOST}" ]]; then
        if [[ "${ARCH}" == "x86_64" ]]; then
            REMOTE_HOST="${VERIFY_WINDOWS_X86_64_HOST:-}"
        else
            REMOTE_HOST="${VERIFY_WINDOWS_ARM64_HOST:-}"
        fi
    fi
    if [[ -z "${REMOTE_HOST}" ]]; then
        echo "ERROR: Remote Windows host required. Pass <user@host> or set VERIFY_WINDOWS_X86_64_HOST / VERIFY_WINDOWS_ARM64_HOST." >&2
        exit 1
    fi

    REL_BIN="${DIST_DIR}/rusty_h265_converter-windows-${ARCH}.exe"
    TEST_BIN="${DIST_DIR}/rusty_h265_converter-tests-windows-${ARCH}.exe"
    REMOTE_DIR_SLASH="C:/Temp/rusty_h265_verify_$$"
    REMOTE_DIR_WIN="C:\\Temp\\rusty_h265_verify_$$"

    cleanup_windows() {
        ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
            "powershell -NoProfile -Command \"Remove-Item -LiteralPath '${REMOTE_DIR_WIN}' -Recurse -Force -ErrorAction SilentlyContinue\"" >/dev/null 2>&1 || true
    }
    trap cleanup_windows EXIT

    echo "================================================================"
    echo "[verify] Remote Windows (${ARCH}) Verification on ${REMOTE_HOST}:${SSH_PORT}"
    echo "================================================================"

    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '${REMOTE_DIR_WIN}\\scripts' | Out-Null\""

    echo "[verify] Deploying release .exe, test .exe, and configs via SCP..."
    scp -q -o BatchMode=yes -o ConnectTimeout=10 -P "${SSH_PORT}" \
        "${REL_BIN}" \
        "${TEST_BIN}" \
        "${REPO_ROOT}/config.yaml" \
        "${REPO_ROOT}/logging.conf" \
        "${REMOTE_HOST}:${REMOTE_DIR_SLASH}/"
    scp -q -o BatchMode=yes -o ConnectTimeout=10 -P "${SSH_PORT}" \
        "${REPO_ROOT}/scripts/rusty_h265_metrics_agent.py" \
        "${REMOTE_HOST}:${REMOTE_DIR_SLASH}/scripts/"

    echo "[verify] 1/3 Release binary --help smoke check..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "powershell -NoProfile -Command \"& '${REMOTE_DIR_WIN}\\rusty_h265_converter-windows-${ARCH}.exe' --help | Out-Null; if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }\""
    echo "[verify]     OK"

    echo "[verify] 2/3 Running base unit tests (tests::) on ${REMOTE_HOST}..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "powershell -NoProfile -Command \"\$env:RUSTY_H265_TEST_ROOT='${REMOTE_DIR_WIN}'; Set-Location '${REMOTE_DIR_WIN}'; & '${REMOTE_DIR_WIN}\\rusty_h265_converter-tests-windows-${ARCH}.exe' tests::; exit \$LASTEXITCODE\""

    echo "[verify] 3/3 Running Windows platform unit tests (windows_tests::) on ${REMOTE_HOST}..."
    ssh -o BatchMode=yes -o ConnectTimeout=10 -p "${SSH_PORT}" "${REMOTE_HOST}" \
        "powershell -NoProfile -Command \"\$env:RUSTY_H265_TEST_ROOT='${REMOTE_DIR_WIN}'; Set-Location '${REMOTE_DIR_WIN}'; & '${REMOTE_DIR_WIN}\\rusty_h265_converter-tests-windows-${ARCH}.exe' windows_tests::; exit \$LASTEXITCODE\""

    echo "[verify] PASS: windows/${ARCH} verification on ${REMOTE_HOST} succeeded."
    exit 0
fi

echo "ERROR: Unsupported platform: ${PLATFORM}" >&2
usage
