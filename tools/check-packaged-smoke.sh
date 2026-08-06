#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/gnosisd-packaged-smoke.XXXXXX")
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

stage() { printf 'packaged smoke: %s\n' "$1" >&2; }
for command in nix curl jq sha256sum; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "$command is required" >&2
        exit 1
    }
done

stage build
package=$(nix build "$project_dir" --no-link --print-out-paths)
gnosisd=$package/bin/gnosisd
gnosisd_admin=$package/bin/gnosisd-admin
test -x "$gnosisd"
test -x "$gnosisd_admin"
test ! -e "$package/bin/gnosis"
count=0
for executable in "$package"/bin/*; do
    test -x "$executable"
    case $(basename "$executable") in
        gnosisd|gnosisd-admin) ;;
        *) echo "unexpected packaged executable: $executable" >&2; exit 1 ;;
    esac
    count=$((count + 1))
done
test "$count" -eq 2
test -f "$package/share/systemd/user/gnosisd.service"
test -f "$package/share/systemd/user/gnosis-backup.service"
grep -q '/gnosisd-admin ' "$package/share/systemd/user/gnosis-backup.service"

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
    ready=0
    count=0
    while test "$count" -lt 100; do
        if body=$(curl --fail --silent "$http/health" 2>/dev/null); then
            test "$body" = '{"status":"ready"}'
            ready=1
            break
        fi
        kill -0 "$daemon_pid" 2>/dev/null || break
        count=$((count + 1))
        sleep 0.05
    done
    test "$ready" -eq 1 && break
    if kill -0 "$daemon_pid" 2>/dev/null; then
        kill -TERM "$daemon_pid" 2>/dev/null || true
    fi
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=
    attempt=$((attempt + 1))
    port=$((port + 1))
done

test -n "$daemon_pid" || {
    cat "$log" >&2
    echo "packaged gnosisd did not become ready" >&2
    exit 1
}

stage openapi
curl --fail --silent --show-error "$http/v1/openapi.json" >"$temporary/openapi.json"
jq -e '
  .openapi == "3.1.0" and .info.title == "Gnosis API" and
  (.paths["/v1/document-mutations"].post.operationId == "post_document-mutations") and
  (.paths["/v1/review/sessions"].post.operationId == "post_review_sessions") and
  (.paths["/v1/sync/snapshot"].get.operationId == "get_sync_snapshot")
' "$temporary/openapi.json" >/dev/null

stage operator
"$gnosisd_admin" --socket "$socket" system-info \
    | jq -e '.protocol_version == 2' >/dev/null
credential=$temporary/device.json
device=$("$gnosisd_admin" --socket "$socket" device-create packaged-smoke \
    --scope read --scope review --scope content:write --scope settings:write \
    --credential-file "$credential")
device_id=$(printf %s "$device" | jq -er .device.device_id)
token=$(jq -er .token "$credential")
curl_auth=$temporary/curl-auth
printf 'header = "Authorization: Bearer %s"\n' "$token" >"$curl_auth"
chmod 600 "$curl_auth"
api=$http/v1

stage asset
asset_source=$temporary/image.png
asset_fetch=$temporary/fetched.png
printf 'packaged managed asset' >"$asset_source"
asset_digest=$(sha256sum "$asset_source" | cut -d ' ' -f 1)
asset_path=assets/$asset_digest/image.png
curl --fail-with-body --silent --show-error -X PUT \
    --config "$curl_auth" -H 'Idempotency-Key: smoke-put-asset' -H 'Content-Type: image/png' \
    --data-binary "@$asset_source" "$api/assets/$asset_digest/image.png" \
    | jq -e --arg digest "$asset_digest" --arg path "$asset_path" '
        .status == "applied" and .operation_id == "smoke-put-asset" and
        .authoritative.value.logical_path == $path and
        .authoritative.value.content_sha256 == $digest and
        .authoritative.value.revision == 1 and .authoritative.value.active == true
      ' >/dev/null
curl --fail-with-body --silent --show-error --config "$curl_auth" \
    "$api/assets/$asset_digest/image.png" >"$asset_fetch"
cmp "$asset_source" "$asset_fetch"

stage content
note=$(curl --fail-with-body --silent --show-error \
    --config "$curl_auth" -H 'Idempotency-Key: smoke-create-document' \
    -H 'Content-Type: application/json' \
    -d "{\"logical_path\":\"smoke.org\",\"content_type\":\"text/org; charset=utf-8\",\"source\":\"#+TITLE: Smoke\\n* Node\\n:PROPERTIES:\\n:ID: smoke-node\\n:END:\\n[[file:$asset_path]]\\n\"}" \
    "$api/document-mutations")
document_id=$(printf %s "$note" | jq -er .authoritative.document_id)
curl --fail-with-body --silent --show-error --config "$curl_auth" "$api/nodes/smoke-node" \
    | jq -e '.node_id == "smoke-node"' >/dev/null

thema=$(curl --fail-with-body --silent --show-error \
    --config "$curl_auth" -H 'Idempotency-Key: smoke-create-thema' \
    -H 'Content-Type: application/json' \
    -d "{\"kind\":\"basic\",\"keimenon\":\"Ready?\",\"hypothesis\":[],\"answer\":[\"yes\"],\"tags\":[\"smoke\"],\"concepts\":[{\"node_id\":\"smoke-node\",\"relation\":\"primary\"}],\"asset_paths\":[\"$asset_path\"]}" \
    "$api/themata")
thema_id=$(printf %s "$thema" | jq -er .authoritative.value.thema_id)

stage review
session=$(curl --fail-with-body --silent --show-error \
    --config "$curl_auth" -H 'Content-Type: application/json' \
    -d "{\"source\":{\"kind\":\"manual\",\"tags\":[],\"topic_node_id\":null,\"manual_ids\":[\"$thema_id\"]},\"target_count\":1}" \
    "$api/review/sessions")
session_id=$(printf %s "$session" | jq -er .session_id)
attempt=$(curl --fail-with-body --silent --show-error --config "$curl_auth" \
    -X POST "$api/review/sessions/$session_id/attempts")
attempt_id=$(printf %s "$attempt" | jq -er .attempt.attempt_id)
printf %s "$attempt" | jq -e \
    '.attempt.revealed_answer == null and (.attempt.presentation.question | contains("yes") | not)' \
    >/dev/null
curl --fail-with-body --silent --show-error \
    --config "$curl_auth" -H 'Content-Type: application/json' \
    -d '{"kind":"text","value":"yes"}' "$api/review/attempts/$attempt_id/answer" \
    | jq -e '.detected_success == true and .revealed_answer == ["yes"]' >/dev/null
curl --fail-with-body --silent --show-error --config "$curl_auth" -X POST \
    "$api/review/attempts/$attempt_id/commit" | jq -e '.event_id | length > 0' >/dev/null

stage backup
backup_json=$("$gnosisd_admin" --socket "$socket" \
    admin backup create --directory "$backups")
backup=$(printf %s "$backup_json" | jq -er .path)
"$gnosisd_admin" --socket "$socket" admin backup verify "$backup" >/dev/null
"$gnosisd_admin" --socket "$socket" admin backup restore-validate "$backup" \
    --temporary-directory "$restore" >/dev/null
test ! -e "$restore"

stage shutdown
kill -TERM "$daemon_pid"
wait "$daemon_pid"
daemon_pid=
rmdir "$runtime/gnosis"
install -d -m 0700 "$runtime/gnosis"

stage cold-restore
printf 'simulated database loss\n' >"$database"
for suffix in -journal -wal -shm; do
    printf 'simulated old sidecar\n' >"$database$suffix"
done
"$gnosisd" --socket "$socket" --database "$database" --blob-root "$blobs" \
    --restore-from "$backup" >"$temporary/rollback-path"
rollback=$(cat "$temporary/rollback-path")
test -f "$rollback/gnosis.sqlite"
test -d "$rollback/blobs"
for suffix in -journal -wal -shm; do
    test -f "$rollback/gnosis.sqlite$suffix"
    test ! -e "$database$suffix"
done
test "$(stat -c %a "$rollback")" = 700

stage restored-state
"$gnosisd" --socket "$socket" --database "$database" \
    --blob-root "$blobs" --http-listen "127.0.0.1:$port" >"$log" 2>&1 &
daemon_pid=$!
count=0
while test "$count" -lt 100; do
    if body=$(curl --fail --silent "$http/health" 2>/dev/null); then
        test "$body" = '{"status":"ready"}'
        break
    fi
    kill -0 "$daemon_pid" 2>/dev/null || {
        cat "$log" >&2
        exit 1
    }
    count=$((count + 1))
    sleep 0.05
done
test "$count" -lt 100
curl --fail-with-body --silent --show-error --config "$curl_auth" "$api/notes/$document_id" \
    | grep -q '#+TITLE: Smoke'
curl --fail-with-body --silent --show-error --config "$curl_auth" "$api/themata/$thema_id" \
    | jq -e --arg asset "$asset_path" \
        '.concepts == [{"node_id":"smoke-node","relation":"primary"}] and .asset_paths == [$asset]' \
        >/dev/null
curl --fail-with-body --silent --show-error --config "$curl_auth" "$api/review/history" \
    | jq -e --arg thema "$thema_id" '.items | any(.thema_id == $thema)' >/dev/null
curl --fail-with-body --silent --show-error --config "$curl_auth" \
    "$api/assets/$asset_digest/image.png" >"$asset_fetch"
cmp "$asset_source" "$asset_fetch"
"$gnosisd_admin" --socket "$socket" device-revoke "$device_id" >/dev/null

kill -TERM "$daemon_pid"
wait "$daemon_pid"
daemon_pid=
test "$(stat -c %a "$database")" = 600
test "$(stat -c %a "$blobs")" = 700

printf 'Packaged smoke passed: %s\n' "$package"
