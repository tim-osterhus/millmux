#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if grep -q '^\[patch\.crates-io\]' Cargo.toml; then
    echo "root Cargo.toml must not carry the terminal width graph" >&2
    exit 1
fi

cargo package --workspace --allow-dirty --no-verify

temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

extract_manifest() {
    local package="$1"
    local version="$2"
    local archive="target/package/${package}-${version}.crate"
    local output="$temp_dir/${package}.toml"
    tar -xOf "$archive" "${package}-${version}/Cargo.toml" > "$output"
    printf '%s\n' "$output"
}

dependency_block() {
    local manifest="$1"
    local dependency="$2"
    awk -v section="[dependencies.${dependency}]" '
        $0 == section { active = 1; print; next }
        active && /^\[/ { exit }
        active { print }
    ' "$manifest"
}

assert_contains() {
    local value="$1"
    local expected="$2"
    local label="$3"
    if [[ "$value" != *"$expected"* ]]; then
        echo "$label missing $expected" >&2
        printf '%s\n' "$value" >&2
        exit 1
    fi
}

assert_equals() {
    local value="$1"
    local expected="$2"
    local label="$3"
    if [[ "$value" != "$expected" ]]; then
        echo "$label differs" >&2
        printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$value" >&2
        exit 1
    fi
}

assert_exact_dependency() {
    local manifest="$1"
    local dependency="$2"
    local version="$3"
    local block
    block="$(dependency_block "$manifest" "$dependency")"
    assert_contains "$block" "version = \"$version\"" "$dependency dependency"
    if [[ "$block" == *"path ="* ]]; then
        echo "$dependency dependency retained a workspace path" >&2
        printf '%s\n' "$block" >&2
        exit 1
    fi
}

assert_registry_dependency() {
    local manifest="$1"
    local dependency="$2"
    local version="$3"
    local package="$4"
    local block
    block="$(dependency_block "$manifest" "$dependency")"
    assert_contains "$block" "version = \"$version\"" "$dependency dependency"
    assert_contains "$block" "package = \"$package\"" "$dependency dependency"
    if [[ "$block" == *"path ="* ]]; then
        echo "$dependency dependency retained a workspace path" >&2
        printf '%s\n' "$block" >&2
        exit 1
    fi
}

terminal_manifest="$(extract_manifest millrace-terminal-vt100 0.16.2)"
core_manifest="$(extract_manifest millrace-sessions-core 0.3.0)"
tui_manifest="$(extract_manifest millrace-sessions-tui 0.3.0)"

assert_contains "$(grep '^name = ' "$terminal_manifest")" \
    'name = "millrace-terminal-vt100"' 'terminal package'
assert_contains "$(grep '^rust-version = ' "$terminal_manifest")" \
    'rust-version = "1.78"' 'terminal Rust version'
assert_contains "$(grep '^homepage = ' "$terminal_manifest")" \
    'homepage = "https://github.com/tim-osterhus/millmux"' 'terminal homepage'
assert_contains "$(grep '^repository = ' "$terminal_manifest")" \
    'repository = "https://github.com/tim-osterhus/millmux"' 'terminal repository'
assert_contains "$(awk '/^\[package.metadata.millmux\]/{active=1; next} active && /^\[/{exit} active{print}' "$terminal_manifest")" \
    'upstream_repository = "https://github.com/doy/vt100-rust"' 'terminal upstream attribution'
terminal_direct_dependencies="$(sed -n 's/^\[dependencies\.\(.*\)\]$/\1/p' "$terminal_manifest")"
assert_equals "$terminal_direct_dependencies" $'itoa\nunicode-segmentation\nunicode-width\nvte' \
    'terminal direct dependency blocks'
assert_exact_dependency "$terminal_manifest" itoa '=1.0.18'
assert_exact_dependency "$terminal_manifest" unicode-segmentation '=1.12.0'
assert_exact_dependency "$terminal_manifest" unicode-width '=0.2.0'
assert_exact_dependency "$terminal_manifest" vte '=0.15.0'
assert_contains "$(dependency_block "$core_manifest" unicode-segmentation)" \
    'version = "=1.12.0"' 'core unicode-segmentation dependency'
assert_registry_dependency "$core_manifest" vt100 '=0.16.2' millrace-terminal-vt100
assert_registry_dependency "$tui_manifest" vt100 '=0.16.2' millrace-terminal-vt100
assert_contains "$(dependency_block "$tui_manifest" ratatui)" \
    'version = "=0.29.0"' 'TUI Ratatui dependency'

if grep -R -q 'unicode-width-0.1.14-millmux\|\[patch\.crates-io\]' "$temp_dir"; then
    echo "packed manifests retain a consumer-local width patch" >&2
    exit 1
fi

echo "packed terminal width graph verified"
