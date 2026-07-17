#!/bin/sh

set -eu

gnosis_dir=${GNOSIS_ELISP_DIR:-/home/thanos/Projects/emacs-lisp/gnosis}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/gnosis-review-parity.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

for file in gnosis.el gnosis-algorithm.el gnosis-custom-values.el \
    gnosis-history-schema.el; do
    if ! test -r "$gnosis_dir/lisp/$file"; then
        echo "review parity requires $gnosis_dir/lisp/$file" >&2
        exit 1
    fi
done

mkdir "$work_dir/lisp"
cp "$gnosis_dir"/lisp/*.el "$work_dir/lisp/"

emacs --batch -Q -L "$work_dir/lisp" \
    --eval '(setq native-comp-jit-compilation nil)' \
    --load "$script_dir/export-review-parity.el" \
    >"$work_dir/live.json"

jq -S . "$project_dir/tests/fixtures/review/parity-v1.json" >"$work_dir/expected.json"
jq -S . "$work_dir/live.json" >"$work_dir/live-sorted.json"
diff -u "$work_dir/expected.json" "$work_dir/live-sorted.json"

if command -v cargo >/dev/null 2>&1; then
    cargo test --quiet --manifest-path "$project_dir/Cargo.toml" --lib \
        domain::grading::tests
    cargo test --quiet --manifest-path "$project_dir/Cargo.toml" --lib \
        domain::scheduling::tests
elif command -v nix >/dev/null 2>&1; then
    nix develop "$project_dir" -c cargo test --quiet --manifest-path "$project_dir/Cargo.toml" \
        --lib domain::grading::tests
    nix develop "$project_dir" -c cargo test --quiet --manifest-path "$project_dir/Cargo.toml" \
        --lib domain::scheduling::tests
else
    echo "cargo or nix is required" >&2
    exit 1
fi
