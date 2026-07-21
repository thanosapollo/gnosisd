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

stage ingestion-dry-run
corpus=$temporary/ingestion-corpus
manifest=$temporary/ingestion-manifest.json
journal=$temporary/ingestion-journal.jsonl
first_report=$temporary/ingestion-first.json
second_report=$temporary/ingestion-second.json
mkdir -p "$corpus"
printf '#+TITLE: Packaged ingestion smoke\n* Node\n' >"$corpus/note.org"
cat >"$manifest" <<'EOF'
{"version":1,"root":"ingestion-corpus","entries":[{"operation_id":"packaged-ingestion-smoke","document_id":null,"expected_revision":null,"logical_path":"note.org","content_type":"text/org; charset=utf-8","source_path":"note.org"}]}
EOF
$gnosis --http-url http://127.0.0.1:1 import-manifest "$manifest" \
    --journal "$journal" --dry-run >"$first_report"
$gnosis --http-url http://127.0.0.1:1 import-manifest "$manifest" \
    --journal "$journal" --dry-run >"$second_report"
cmp "$first_report" "$second_report"
test ! -e "$journal"
grep -q '"version":1' "$first_report"
grep -q '"entry_count":1' "$first_report"

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
    --scope read --scope review --scope content:write --scope settings:write)
device_id=$(printf %s "$device" | jq -er .device.device_id)
token=$(printf %s "$device" | jq -er .token)
api=$http/v1
auth=$(printf '%s%s' 'Authorization: Bearer ' "$token")

stage asset
token_file=$temporary/device.token
printf '%s\n' "$token" >"$token_file"
chmod 600 "$token_file"

stage ingestion-apply
apply_report=$temporary/apply.json
replay_report=$temporary/replay.json
"$gnosis" --http-url "$http" --token-file "$token_file" \
    import-manifest "$manifest" --journal "$journal" >"$apply_report"
"$gnosis" --http-url "$http" --token-file "$token_file" \
    import-manifest "$manifest" --journal "$journal" >"$replay_report"
jq -e '.entry_count == 1 and .completed_count == 1 and (.path_hashes | length) == 1' \
    "$apply_report" >/dev/null
cmp "$apply_report" "$replay_report"
jq -e '.version == 1 and (.completed | keys) == ["packaged-ingestion-smoke"]' \
    "$journal" >/dev/null
imported_document_id=$(jq -er '.completed["packaged-ingestion-smoke"].authoritative.document_id' "$journal")
imported_content_type=$(curl --fail-with-body --silent --show-error -H "$auth" \
    --output "$temporary/imported-source.org" --write-out '%{content_type}' \
    "$api/notes/$imported_document_id")
test "$imported_content_type" = 'text/org; charset=utf-8'
cmp "$corpus/note.org" "$temporary/imported-source.org"

asset_source=$temporary/image.png
asset_fetch=$temporary/fetched.png
asset_blob=$temporary/blob.png
printf 'packaged managed asset' >"$asset_source"
asset_size=$(wc -c <"$asset_source" | tr -d ' ')
asset_digest=$(sha256sum "$asset_source" | cut -d ' ' -f 1)
asset_path=assets/$asset_digest/image.png
asset_put=$($gnosis --http-url "$http" --token-file "$token_file" \
    asset put "$asset_digest" image.png --operation-id smoke-put-asset \
    --media-type image/png <"$asset_source")
printf %s "$asset_put" | jq -e \
    --arg digest "$asset_digest" --arg path "$asset_path" \
    '.status == "applied" and .operation_id == "smoke-put-asset" and
     .authoritative.kind == "asset" and .authoritative.value.logical_path == $path and
     .authoritative.value.content_sha256 == $digest and
     .authoritative.value.media_type == "image/png" and
     .authoritative.value.revision == 1 and .authoritative.value.active == true' >/dev/null
$gnosis --http-url "$http" --token-file "$token_file" \
    asset fetch "$asset_digest" image.png --revision 1 \
    --size-bytes "$asset_size" --media-type image/png >"$asset_fetch"
cmp "$asset_source" "$asset_fetch"
$gnosis --http-url "$http" --token-file "$token_file" \
    asset blob "$asset_digest" >"$asset_blob"
cmp "$asset_source" "$asset_blob"

stage content
note=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Idempotency-Key: smoke-create-document' \
    -H 'Content-Type: application/json' \
    -d "{\"logical_path\":\"smoke.org\",\"content_type\":\"text/org; charset=utf-8\",\"source\":\"#+TITLE: Smoke\\n* Node\\n:PROPERTIES:\\n:ID: smoke-node\\n:END:\\nSee [[id:smoke-linked][Linked]].\\n[[file:$asset_path]]\\n* Linked\\n:PROPERTIES:\\n:ID: smoke-linked\\n:END:\\n\"}" \
    "$api/document-mutations")
document_id=$(printf %s "$note" | jq -er .authoritative.document_id)
curl --fail-with-body --silent --show-error -H "$auth" \
    "$api/nodes/smoke-node" | jq -e '.node_id == "smoke-node"' >/dev/null

thema=$(curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Idempotency-Key: smoke-create-thema' \
    -H 'Content-Type: application/json' \
    -d "{\"kind\":\"basic\",\"keimenon\":\"Ready?\",\"hypothesis\":[],\"answer\":[\"yes\"],\"tags\":[\"smoke\"],\"concepts\":[{\"node_id\":\"smoke-node\",\"relation\":\"primary\"}],\"asset_paths\":[\"$asset_path\"]}" \
    "$api/themata")
thema_id=$(printf %s "$thema" | jq -er .authoritative.value.thema_id)

stage settings
curl --fail-with-body --silent --show-error \
    -H "$auth" -H 'Idempotency-Key: smoke-settings' -H 'If-Match: "1"' \
    -H 'Content-Type: application/json' -X PUT \
    -d '{"timezone":"Europe/Athens","day_boundary_hour":4,"desired_retention":0.9}' \
    "$api/settings" | jq -e '.changed == true' >/dev/null

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

stage backup
backup_json=$($gnosis --socket "$socket" admin backup create --directory "$backups")
backup=$(printf %s "$backup_json" | jq -er .path)
$gnosis --socket "$socket" admin backup verify "$backup" >/dev/null
$gnosis --socket "$socket" admin backup restore-validate "$backup" \
    --temporary-directory "$restore" >/dev/null
test ! -e "$restore"

stage shutdown
kill -TERM "$daemon_pid"
wait "$daemon_pid"
daemon_pid=
rmdir "$runtime/gnosis"
install -d -m 0700 "$runtime/gnosis"
test "$(stat -c %a "$runtime/gnosis")" = 700

stage cold-restore
printf 'simulated database loss\n' >"$database"
printf 'simulated old journal\n' >"$database-journal"
printf 'simulated old wal\n' >"$database-wal"
printf 'simulated old shm\n' >"$database-shm"
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

stage restart-restored
"$gnosisd" --socket "$socket" --database "$database" \
    --blob-root "$blobs" --http-listen "127.0.0.1:$port" \
    >"$log" 2>&1 &
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

stage verify-restored-state
curl --fail-with-body --silent --show-error -H "$auth" "$api/notes/$document_id" \
    | grep -q '#+TITLE: Smoke'
curl --fail-with-body --silent --show-error -H "$auth" "$api/nodes/smoke-node" \
    | jq -e '.node_id == "smoke-node"' >/dev/null
curl --fail-with-body --silent --show-error -H "$auth" "$api/nodes/smoke-linked" \
    | jq -e '.node_id == "smoke-linked"' >/dev/null
curl --fail-with-body --silent --show-error -H "$auth" "$api/nodes/smoke-node/outbound" \
    | jq -e '.items | map(.node_id) == ["smoke-linked"]' >/dev/null
curl --fail-with-body --silent --show-error -H "$auth" "$api/themata/$thema_id" \
    | jq -e --arg asset "$asset_path" \
        '.concepts == [{"node_id":"smoke-node","relation":"primary"}] and .asset_paths == [$asset]' \
        >/dev/null
$gnosis --http-url "$http" --token-file "$token_file" \
    asset fetch "$asset_digest" image.png --revision 1 \
    --size-bytes "$asset_size" --media-type image/png >"$asset_fetch"
cmp "$asset_source" "$asset_fetch"
curl --fail-with-body --silent --show-error -H "$auth" "$api/settings" \
    | jq -e '.revision == 2 and .timezone == "Europe/Athens" and .day_boundary_hour == 4' \
        >/dev/null
curl --fail-with-body --silent --show-error -H "$auth" "$api/review/history" \
    | jq -e --arg thema "$thema_id" '.items | any(.thema_id == $thema)' >/dev/null

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

stage cleanup-asset
asset_deleted=$($gnosis --http-url "$http" --token-file "$token_file" \
    asset tombstone "$asset_digest" image.png --operation-id smoke-delete-asset \
    --if-revision 1)
printf %s "$asset_deleted" | jq -e \
    '.status == "applied" and .operation_id == "smoke-delete-asset" and
     .authoritative.kind == "asset" and .authoritative.value.revision == 2 and
     .authoritative.value.active == false and .authoritative.value.content_sha256 == null and
     .authoritative.value.media_type == null' >/dev/null
if $gnosis --http-url "$http" --token-file "$token_file" \
    asset fetch "$asset_digest" image.png --revision 2 \
    --size-bytes "$asset_size" --media-type image/png \
    >"$asset_fetch" 2>/dev/null; then
    echo "tombstoned asset remained accessible" >&2
    exit 1
fi
$gnosis --http-url "$http" --token-file "$token_file" \
    asset blob "$asset_digest" >"$asset_blob"
cmp "$asset_source" "$asset_blob"
$gnosis --socket "$socket" device-revoke "$device_id" >/dev/null

kill -TERM "$daemon_pid"
wait "$daemon_pid"
daemon_pid=
test "$(stat -c %a "$database")" = 600
test "$(stat -c %a "$blobs")" = 700

printf 'Packaged smoke passed: %s\n' "$package"
