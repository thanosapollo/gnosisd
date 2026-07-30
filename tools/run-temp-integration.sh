#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
core_dir=${GNOSIS_CORE_DIR:-$project_dir/../../rust/gnosis-core}
gnosisd=${GNOSISD:-}
gnosis_cli=${GNOSIS_CLI:-}
emacs_bin=$(command -v "${EMACS:-emacs}")
make_bin=${MAKE:-make}

if [ -n "$gnosisd" ] || [ -n "$gnosis_cli" ]; then
    if [ -z "$gnosisd" ] || [ -z "$gnosis_cli" ]; then
        echo "GNOSISD and GNOSIS_CLI must be set together" >&2
        exit 1
    fi
else
    [ -f "$core_dir/Cargo.toml" ] || {
        echo "Set GNOSIS_CORE_DIR or GNOSISD and GNOSIS_CLI" >&2
        exit 1
    }
    nix develop "$core_dir" --command \
        cargo build --manifest-path "$core_dir/Cargo.toml" --bins
    gnosisd=$core_dir/target/debug/gnosisd
    gnosis_cli=$core_dir/target/debug/gnosis
fi

[ -x "$gnosisd" ] || { echo "gnosisd is not executable: $gnosisd" >&2; exit 1; }
[ -x "$gnosis_cli" ] || { echo "gnosis is not executable: $gnosis_cli" >&2; exit 1; }

suite_state=$(mktemp -d "${TMPDIR:-/tmp}/gnosis-emacs-contract.XXXXXX")
chmod 700 "$suite_state"
state=
pid=
cleanup() {
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$suite_state"
}
trap cleanup EXIT HUP INT TERM

cd "$project_dir"
tests=$(sed -n \
    's/^(ert-deftest \(gnosis-core-integration-[^ ]*\) ().*/\1/p' \
    tests/gnosis-test-core-integration.el)
[ -n "$tests" ] || { echo "No integration tests found" >&2; exit 1; }

for test_name in $tests; do
    state=$suite_state/$test_name
    mkdir -m 700 "$state"
    attempt=0
    while :; do
        port=$(shuf -i 20000-59999 -n 1)
        if ! curl --silent --max-time 1 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
            break
        fi
        attempt=$((attempt + 1))
        [ "$attempt" -lt 20 ] || { echo "Unable to select integration port" >&2; exit 1; }
    done

    "$gnosisd" \
        --socket "$state/gnosis.sock" \
        --database "$state/gnosis.db" \
        --blob-root "$state/blobs" \
        --http-listen "127.0.0.1:$port" \
        >"$state/gnosisd.log" 2>&1 &
    pid=$!

    attempt=0
    until curl --fail --silent --max-time 1 "http://127.0.0.1:$port/health" >/dev/null 2>&1; do
        if ! kill -0 "$pid" 2>/dev/null; then
            sed -n '1,120p' "$state/gnosisd.log" >&2
            exit 1
        fi
        attempt=$((attempt + 1))
        [ "$attempt" -lt 100 ] || { echo "Temporary gnosisd did not become ready" >&2; exit 1; }
        sleep 0.05
    done

    device=$(
        "$gnosis_cli" --socket "$state/gnosis.sock" \
            device-create emacs-contract --scope read --scope review \
            --scope content:write
    )
    token=$(printf '%s\n' "$device" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    device_2=$(
        "$gnosis_cli" --socket "$state/gnosis.sock" \
            device-create emacs-contract-2 --scope read --scope review \
            --scope content:write
    )
    token_2=$(printf '%s\n' "$device_2" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    [ -n "$token" ] && [ -n "$token_2" ] || {
        echo "Temporary device tokens missing" >&2
        exit 1
    }

    openapi=$state/openapi.json
    generated=$state/gnosis-openapi-contract.el
    curl --fail --show-error --silent \
        "http://127.0.0.1:$port/v1/openapi.json" >"$openapi"
    GNOSIS_ENV_WRAPPED=1 \
        "$make_bin" --no-print-directory \
        OPENAPI_FILE="$openapi" OPENAPI_OUTPUT="$generated" do-openapi
    "$emacs_bin" --batch -Q -f batch-byte-compile "$generated"

    GNOSIS_ENV_WRAPPED=1 \
    GNOSIS_HTTP_TEST_URL="http://127.0.0.1:$port" \
    GNOSIS_HTTP_TEST_TOKEN="$token" \
    GNOSIS_HTTP_TEST_TOKEN_2="$token_2" \
    GNOSIS_OPENAPI_GENERATED="$generated" \
        "$make_bin" --no-print-directory EMACS="$emacs_bin" \
        TEST=tests/gnosis-test-core-integration.el \
        ERT_SELECTOR="$test_name" do-test

    kill "$pid"
    wait "$pid"
    pid=
done
