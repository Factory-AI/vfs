#!/usr/bin/env bash
set -u

usage() {
    cat <<'USAGE'
Usage: check-fork-governance.sh

Read-only Phase 1 fork governance check.

Verifies that the current checkout's origin remote points at the
Factory-AI/vfs fork, reports branch metadata, and warns if the local
upstream-main or factory-main branch names are absent.

Exit codes:
  0  origin looks like Factory-AI/vfs
  1  git is unavailable or origin does not look like Factory-AI/vfs
USAGE
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

fail() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'WARNING: %s\n' "$*" >&2
}

if ! command -v git >/dev/null 2>&1; then
    fail "git is not available on PATH"
fi

if ! git -C "$repo_root" rev-parse --git-dir >/dev/null 2>&1; then
    fail "$repo_root is not a git checkout"
fi

origin_url="$(git -C "$repo_root" config --get remote.origin.url 2>/dev/null || true)"
if [ -z "$origin_url" ]; then
    fail "remote.origin.url is not configured"
fi

origin_lc="$(printf '%s' "$origin_url" | tr '[:upper:]' '[:lower:]')"
normalized_origin="$origin_lc"
case "$normalized_origin" in
    git@github.com:*)
        normalized_origin="${normalized_origin#git@github.com:}"
        ;;
    ssh://git@github.com/*)
        normalized_origin="${normalized_origin#ssh://git@github.com/}"
        ;;
    https://github.com/*)
        normalized_origin="${normalized_origin#https://github.com/}"
        ;;
    http://github.com/*)
        normalized_origin="${normalized_origin#http://github.com/}"
        ;;
esac
normalized_origin="${normalized_origin%.git}"

if [ "$normalized_origin" != "factory-ai/vfs" ]; then
    fail "origin does not exactly match Factory-AI/vfs: $origin_url"
fi

current_branch="$(git -C "$repo_root" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
if [ -z "$current_branch" ]; then
    current_branch="DETACHED@$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"
fi

default_remote_head="$(git -C "$repo_root" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)"
if [ -z "$default_remote_head" ]; then
    default_remote_head="not configured locally"
fi

printf 'Phase 1 fork governance check\n'
printf 'Repository: %s\n' "$repo_root"
printf 'Origin: %s\n' "$origin_url"
printf 'Current branch: %s\n' "$current_branch"
printf 'Origin default HEAD: %s\n' "$default_remote_head"

for branch_name in upstream-main factory-main; do
    if git -C "$repo_root" show-ref --verify --quiet "refs/heads/$branch_name"; then
        printf 'Local branch %s: present\n' "$branch_name"
    else
        warn "local branch '$branch_name' is not present"
        printf 'Local branch %s: missing (warning only)\n' "$branch_name"
    fi
done

printf 'Result: origin matches Factory-AI/vfs\n'
