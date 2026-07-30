#!/bin/sh

set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 ORG_FIXTURE" >&2
    exit 2
fi

fixture=$1
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
gnosis_dir=${GNOSIS_ELISP_DIR:-$project_dir/clients/emacs}
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/gnosis-org-parity.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

if [ ! -r "$gnosis_dir/lisp/gnosis-org.el" ]; then
    echo "cannot read $gnosis_dir/lisp/gnosis-org.el; set GNOSIS_ELISP_DIR" >&2
    exit 1
fi

case $fixture in
    /*) fixture_path=$fixture ;;
    *) fixture_path=$project_dir/$fixture ;;
esac

export GNOSIS_PARITY_FIXTURE=$fixture_path
export GNOSIS_PARITY_OUTPUT=$work_dir/rust.json

if command -v cargo >/dev/null 2>&1; then
    cargo test --quiet --manifest-path "$project_dir/Cargo.toml" \
        --lib parser::org_tests::export_parity_projection -- --ignored --exact
elif command -v nix >/dev/null 2>&1; then
    nix shell nixpkgs#cargo nixpkgs#rustc -c \
        cargo test --quiet --manifest-path "$project_dir/Cargo.toml" \
        --lib parser::org_tests::export_parity_projection -- --ignored --exact
else
    echo "cargo or nix is required" >&2
    exit 1
fi

emacs --batch -Q \
    -L "$gnosis_dir/lisp" \
    --eval '(setq native-comp-jit-compilation nil)' \
    --load "$gnosis_dir/lisp/gnosis-org.el" \
    --load "$script_dir/export-gnosis-org-fixture.el" \
    >"$work_dir/elisp.json"

jq -S . "$work_dir/rust.json" >"$work_dir/rust.sorted.json"
jq -S . "$work_dir/elisp.json" >"$work_dir/elisp.sorted.json"
diff -u "$work_dir/elisp.sorted.json" "$work_dir/rust.sorted.json"
