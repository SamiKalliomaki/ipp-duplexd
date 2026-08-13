#!/bin/bash
# Cut a release: bump Cargo.toml + Cargo.lock, run the CI checks, commit,
# tag v<ver>, and push — the Release workflow then builds and publishes
# the packages. Usage: packaging/release.sh 0.1.3
set -euo pipefail
cd "$(dirname "$0")/.."

ver=${1:-}
if [[ ! "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "usage: $0 <version>    e.g. $0 0.1.3" >&2
    exit 2
fi
if ! command -v shellcheck >/dev/null; then
    echo "shellcheck is not installed" >&2
    echo "  e.g. sudo pacman -S shellcheck  /  sudo apt install shellcheck" >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v$ver" >/dev/null; then
    echo "tag v$ver already exists" >&2
    exit 1
fi
# the release commit should hold nothing but the version bump
if [[ $(jj log -r @ --no-graph -T 'empty') != "true" ]]; then
    echo "working copy (@) has changes — commit or abandon them first" >&2
    exit 1
fi
# ...and go on top of main, so moving the bookmark forward is all that is left
if [[ $(jj log -r main --no-graph -T 'commit_id') != $(jj log -r @- --no-graph -T 'commit_id') ]]; then
    echo "main is not at @- — releases are cut from the tip of main:" >&2
    jj log -r 'main | @-' >&2
    exit 1
fi

echo "==> bumping version to $ver"
sed -i "s/^version = \".*\"/version = \"$ver\"/" Cargo.toml
cargo update --offline -p ipp-duplexd

echo "==> running the CI checks"
# same list as .github/workflows/ci.yml, warnings denied like there
export RUSTFLAGS="-D warnings"
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
shellcheck packaging/*.sh
cargo build --all-targets --locked
cargo test --locked

echo "==> committing and tagging"
jj commit -m "Release v$ver"
jj tag set "v$ver" -r @-
jj bookmark set main -r @-

jj log -r @- --no-graph
read -r -p "push main and v$ver to origin? [y/N] " answer
if [[ "$answer" != [yY] ]]; then
    echo "not pushed — when ready: jj git push --bookmark main --tag v$ver"
    exit 0
fi
jj git push --bookmark main --tag "v$ver"

echo
echo "done — the Release workflow is building the packages:"
echo "  https://github.com/SamiKalliomaki/ipp-duplexd/actions"
