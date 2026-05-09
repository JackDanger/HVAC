#!/usr/bin/env bash
set -euo pipefail

REMOTE_HOST="root@10.30.0.199"
SSH_OPTS="-J neurotic"
REMOTE_DIR="/root/tdorr"
PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"

sync_code() {
    echo "==> Syncing code to ${REMOTE_HOST}:${REMOTE_DIR}"
    rsync -az --delete \
        --exclude target/ \
        --exclude .git/ \
        -e "ssh ${SSH_OPTS}" \
        "${PROJECT_DIR}/" \
        "${REMOTE_HOST}:${REMOTE_DIR}/"
}

remote_cmd() {
    ssh ${SSH_OPTS} "${REMOTE_HOST}" "cd ${REMOTE_DIR} && $*"
}

case "${1:-build}" in
    build)
        sync_code
        echo "==> Building on remote host"
        remote_cmd make build
        ;;
    test)
        sync_code
        echo "==> Running tests on remote host"
        remote_cmd make test
        ;;
    run)
        sync_code
        echo "==> Building and running on remote host"
        shift
        # skip the -- separator if present
        [[ "${1:-}" == "--" ]] && shift
        remote_cmd make build
        remote_cmd "./target/release/hvecuum $*"
        ;;
    sync)
        sync_code
        ;;
    shell)
        ssh ${SSH_OPTS} "${REMOTE_HOST}" -t "cd ${REMOTE_DIR} && bash"
        ;;
    *)
        echo "Usage: $0 {build|test|run [-- args...]|sync|shell}"
        exit 1
        ;;
esac
