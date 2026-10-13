#!/bin/sh
# Build (if stale) and run the jira-tern binary from this plugin directory.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
plugin_dir=$(dirname -- "$script_dir")
bin="$plugin_dir/target/release/jira-tern"

needs_build=0
if [ ! -x "$bin" ]; then
    needs_build=1
else
    for f in "$plugin_dir"/Cargo.toml "$plugin_dir"/Cargo.lock "$plugin_dir"/src/*.rs; do
        if [ -e "$f" ] && [ "$f" -nt "$bin" ]; then
            needs_build=1
            break
        fi
    done
fi

if [ "$needs_build" -eq 1 ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "jira-tern: cargo not found; install a Rust toolchain (>= 1.89)" >&2
        exit 127
    fi
    echo "jira-tern: building (cargo build --release --locked)..." >&2
    cargo build --release --locked \
        --manifest-path "$plugin_dir/Cargo.toml" \
        --target-dir "$plugin_dir/target" >&2
    touch "$bin"
fi

# A pane opened from the palette closes when the program exits, hiding its
# message; on failure keep it open until the user has read it.
status=0
"$bin" "$@" || status=$?
if [ "$status" -ne 0 ] && [ -t 0 ]; then
    printf '\njira-tern exited with %s. Press Enter to close.' "$status" >&2
    read -r _ || true
fi
exit "$status"
