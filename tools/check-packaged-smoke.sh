#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/gnosis-packaged-smoke.XXXXXX")
daemon_pid=

cleanup() {
    if test -n "$daemon_pid" && kill -0 "$daemon_pid" 2>/dev/null; then
        kill -TERM "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$temporary"
}

trap cleanup EXIT HUP INT TERM
chmod 700 "$temporary"

stage() {
    printf 'packaged smoke: %s\n' "$1" >&2
}

for command in nix curl jq; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "$command is required" >&2
        exit 1
    }
done

stage build
package=$(nix build "$project_dir" --no-link --print-out-paths)
gnosisd=$package/bin/gnosisd
gnosis=$package/bin/gnosis
test -x "$gnosisd"
test -x "$gnosis"

runtime=$temporary/runtime
socket=$runtime/gnosis/gnosis.sock
database=$temporary/data/gnosis.sqlite
blobs=$temporary/data/blobs
backups=$temporary/backups
restore=$temporary/restore
log=$temporary/gnosisd.log
mkdir -p "$runtime/gnosis" "$temporary/data" "$backups"
chmod 700 "$runtime" "$runtime/gnosis" "$temporary/data" "$backups"

port=$((20000 + ($$ % 20000)))
attempt=0
while test "$attempt" -lt 20; do
    http=http://127.0.0.1:$port
    "$gnosisd" --socket "$socket" --database "$database" \
        --blob-root "$blobs" --http-listen "127.0.0.1:$port" \
        >"$log" 2>&1 &
    daemon_pid=$!
    count=0
    while test "$count" -lt 100; do
        if body=$(curl --fail --silent "$http/health" 2>/dev/null); then
            test "$body" = '{"status":"ready"}'
            break 2
        fi
        if ! kill -0 "$daemon_pid" 2>/dev/null; then
            wait "$daemon_pid" 2>/dev/null || true
            daemon_pid=
            break
        fi
        count=$((count + 1))
        sleep 0.05
    done
    if test -n "$daemon_pid"; then
        kill -TERM "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
        daemon_pid=
    fi
    attempt=$((attempt + 1))
    port=$((port + 1))
done

test -n "$daemon_pid" || {
    cat "$log" >&2
    echo "packaged gnosisd did not become ready" >&2
    exit 1
}

stage device
device=$($gnosis --socket "$socket" device-create packaged-smoke \
    --scope read --scope review --scope content:write)
device_id=$(printf %s "$device" | jq -er .device.device_id)
token=$(printf %s "$device" | jq -er .token)
api=$http/v1
auth="Authorization: Bearer $token"

stage content
note=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Idempotency-Key: smoke-create-document' \
    -H 'Content-Type: application/json' \
    -d '{"logical_path":"smoke.org","org":"#+TITLE: Smoke\n* Node\n:PROPERTIES:\n:ID: smoke-node\n:END:\n"}' \
    "$api/document-mutations")
document_id=$(printf %s "$note" | jq -er .authoritative.document_id)
curl --fail-with-body --silent --show-error -H "$auth" \
    "$api/nodes/smoke-node" | jq -e '.node_id == "smoke-node"' >/dev/null

thema=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Idempotency-Key: smoke-create-thema' \
    -H 'Content-Type: application/json' \
    -d '{"kind":"basic","keimenon":"Ready?","hypothesis":[],"answer":["yes"],"tags":["smoke"],"concepts":[{"node_id":"smoke-node","relation":"primary"}],"asset_paths":[]}' \
    "$api/themata")
thema_id=$(printf %s "$thema" | jq -er .authoritative.value.thema_id)

stage review
session=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Content-Type: application/json' \
    -d "{\"source\":{\"kind\":\"manual\",\"tags\":[],\"topic_node_id\":null,\"manual_ids\":[\"$thema_id\"]},\"target_count\":1}" \
    "$api/review/sessions")
session_id=$(printf %s "$session" | jq -er .session_id)
stage review-begin
attempt_json=$(curl --fail-with-body --silent --show-error -H "$auth" \
    -X POST "$api/review/sessions/$session_id/attempts")
attempt_id=$(printf %s "$attempt_json" | jq -er .attempt_id)
printf %s "$attempt_json" | jq -e \
    '.revealed_answer == null and (.presentation.question | contains("yes") | not)' \
    >/dev/null

stage review-grade
graded=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Content-Type: application/json' \
    -d '{"kind":"text","value":"yes"}' \
    "$api/review/attempts/$attempt_id/answer")
printf %s "$graded" | jq -e \
    '.detected_success == true and .accepted_success == true and .revealed_answer == ["yes"]' \
    >/dev/null
stage review-commit
curl --fail-with-body --silent --show-error -H "$auth" -X POST \
    "$api/review/attempts/$attempt_id/commit" | jq -e '.event_id | length > 0' >/dev/null

stage cleanup-content
curl --fail-with-body --silent --show-error -H "$auth" \
    -H 'Idempotency-Key: smoke-archive-thema' -H 'If-Match: "1"' \
    -X POST "$api/themata/$thema_id/archive" \
    | jq -e '.authoritative.value.archived == true' >/dev/null
stage cleanup-note
curl --fail-with-body --silent --show-error -H "$auth" \
    -H 'Idempotency-Key: smoke-delete-document' -H 'If-Match: "1"' \
    -X DELETE "$api/document-mutations/$document_id" \
    | jq -e '.authoritative.active == false' >/dev/null

stage backup
backup_json=$($gnosis --socket "$socket" admin backup create --directory "$backups")
backup=$(printf %s "$backup_json" | jq -er .path)
$gnosis --socket "$socket" admin backup verify "$backup" >/dev/null
$gnosis --socket "$socket" admin backup restore-validate "$backup" \
    --temporary-directory "$restore" >/dev/null
test ! -e "$restore"
$gnosis --socket "$socket" device-revoke "$device_id" >/dev/null

stage shutdown
kill -TERM "$daemon_pid"
wait "$daemon_pid"
daemon_pid=
test "$(stat -c %a "$database")" = 600
test "$(stat -c %a "$blobs")" = 700

printf 'Packaged smoke passed: %s\n' "$package"
