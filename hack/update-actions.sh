#!/usr/bin/env bash
# Scans workflow files for GitHub Actions, resolves the latest release,
# and prints the pinned hash reference.
#
# Usage: ./hack/update-actions.sh [--check|--update]
#   --check    Exit non-zero if an action is stale or cannot be resolved
#   --update   Apply the pinned hashes to the workflow files in-place
# Requires: gh (GitHub CLI), authenticated; curl; jq

set -euo pipefail

do_update=false
do_check=false
check_added_range=""
case "${1:-}" in
    "")
        ;;
    --check)
        do_check=true
        ;;
    --update)
        do_update=true
        ;;
    --check-added)
        if [[ $# -ne 2 ]]; then
            echo "Usage: $0 --check-added <git-revision-range>" >&2
            exit 2
        fi
        do_check=true
        check_added_range=$2
        ;;
    *)
        echo "Usage: $0 [--check|--update|--check-added <git-revision-range>]" >&2
        exit 2
        ;;
esac

for command in gh curl jq; do
    if ! command -v "$command" &>/dev/null; then
        echo "Error: $command is required." >&2
        exit 1
    fi
done

workflow_dir="$(git rev-parse --show-toplevel)/.github/workflows"

# Extract unique action references (owner/repo@version) from all workflow files,
# or only references added by the requested diff.
action_pattern="uses:\\s*['\"]?\\K[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+(?:/[a-zA-Z0-9_.-]+)*@[^\\s'\",}]+"
if [[ -n "$check_added_range" ]]; then
    added_lines=$(git diff --unified=0 "$check_added_range" -- .github/workflows)
    mapfile -t actions < <(
        printf '%s\n' "$added_lines" \
            | grep -E '^\+[^+]' \
            | grep -oP "$action_pattern" \
            | sort -u || true
    )
else
    mapfile -t actions < <(
        grep -rhoP "$action_pattern" "$workflow_dir" | sort -u
    )
fi

if [[ ${#actions[@]} -eq 0 ]]; then
    if [[ -n "$check_added_range" ]]; then
        echo "No new action references found in $check_added_range"
    else
        echo "No actions found in $workflow_dir"
    fi
    exit 0
fi

# Resolve latest release + commit hash for each action
declare -A latest_tag latest_hash

sed_escape_regex() {
    printf '%s' "$1" | sed -E 's/[][\\.^$*+?{}()|]/\\&/g'
}

github_api() {
    local endpoint=$1
    local response

    if response=$(gh api "$endpoint" 2>/dev/null); then
        printf '%s\n' "$response"
        return
    fi

    curl --fail --silent --show-error \
        --header "Accept: application/vnd.github+json" \
        --header "X-GitHub-Api-Version: 2022-11-28" \
        "https://api.github.com/$endpoint"
}

resolve_tag_sha() {
    local repo=$1 tag=$2
    local ref_data obj_type obj_sha

    if ! ref_data=$(github_api "repos/$repo/git/ref/tags/$tag"); then
        echo "(not found)"
        return
    fi
    obj_type=$(jq -r '.object.type // empty' <<< "$ref_data")
    obj_sha=$(jq -r '.object.sha // empty' <<< "$ref_data")

    # Dereference annotated tags to get the commit
    if [[ "$obj_type" == "tag" ]]; then
        if ! ref_data=$(github_api "repos/$repo/git/tags/$obj_sha"); then
            echo "(not found)"
            return
        fi
        obj_sha=$(jq -r '.object.sha // empty' <<< "$ref_data")
    fi

    if [[ "$obj_sha" =~ ^[0-9a-f]{40}$ ]]; then
        echo "$obj_sha"
    else
        echo "(not found)"
    fi
}

for entry in "${actions[@]}"; do
    action="${entry%%@*}"
    remainder="${action#*/}"
    repo="${action%%/*}/${remainder%%/*}"
    current="${entry##*@}"

    # Get the newest release (any major version)
    release_data=$(github_api "repos/$repo/releases?per_page=100") || release_data=""
    release=$(jq -r '.[] | select(.draft == false and .prerelease == false) | .tag_name' <<< "$release_data" 2>/dev/null \
        | grep -E '^v[0-9]' | sort -V | tail -1) || true

    if [[ -z "$release" ]]; then
        release="$current"
    fi

    latest_tag["$entry"]="$release"
    latest_hash["$entry"]=$(resolve_tag_sha "$repo" "$release")
done

markdown_escape() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//|/\\|}
    printf '%s' "$value"
}

# Print summary table
echo "| ACTION | CURRENT | LATEST | LATEST HASH | UPDATE? | URL |"
echo "| --- | --- | --- | --- | --- | --- |"

outdated=0
unresolved=0
for entry in "${actions[@]}"; do
    action="${entry%%@*}"
    remainder="${action#*/}"
    repo="${action%%/*}/${remainder%%/*}"
    current="${entry##*@}"
    if [[ "${latest_hash[$entry]}" == "(not found)" ]]; then
        needs_update="ERROR"
        unresolved=$((unresolved + 1))
    elif [[ "$current" != "${latest_hash[$entry]}" ]]; then
        needs_update="YES"
        outdated=$((outdated + 1))
    else
        needs_update="no"
    fi

    printf '| %s | %s | %s | %s | %s | %s |\n' \
        "$(markdown_escape "$repo")" \
        "$(markdown_escape "$current")" \
        "$(markdown_escape "${latest_tag[$entry]}")" \
        "$(markdown_escape "${latest_hash[$entry]}")" \
        "$needs_update" \
        "$(markdown_escape "https://github.com/$repo/releases")"
done

# Print pinned references
echo ""
echo "# Pinned references (copy into workflows):"
for entry in "${actions[@]}"; do
    action="${entry%%@*}"
    hash="${latest_hash[$entry]}"
    tag="${latest_tag[$entry]}"
    if [[ "$hash" != "(not found)" ]]; then
        echo "  $action@$hash  # $tag"
    fi
done

if [[ "$do_check" == true ]]; then
    if [[ $unresolved -ne 0 ]]; then
        echo "Error: could not resolve $unresolved action reference(s)." >&2
        exit 1
    fi
    if [[ $outdated -ne 0 ]]; then
        echo "Error: $outdated action reference(s) are not current." >&2
        exit 1
    fi
    echo ""
    echo "All action references are current."
fi

# Apply updates to workflow files
if [[ "$do_update" == true ]]; then
    echo ""
    updated=0
    for entry in "${actions[@]}"; do
        action="${entry%%@*}"
        current="${entry##*@}"
        hash="${latest_hash[$entry]}"
        tag="${latest_tag[$entry]}"
        escaped_action=$(sed_escape_regex "$action")
        escaped_current=$(sed_escape_regex "$current")

        if [[ "$hash" == "(not found)" || "$current" == "$hash" ]]; then
            continue
        fi

        # Replace only the scalar value so quoted and flow-style YAML stays valid.
        while IFS= read -r file; do
            if grep -Fq "${action}@${current}" "$file"; then
                sed -i -E "s|${escaped_action}@${escaped_current}|${action}@${hash}|g" "$file"
                echo "Updated $action in $(basename "$file"): ${current} -> ${hash} (${tag})"
                updated=$((updated + 1))
            fi
        done < <(find "$workflow_dir" -name '*.yml' -o -name '*.yaml')
    done

    if [[ $updated -eq 0 ]]; then
        echo "No updates needed."
    else
        echo ""
        echo "Updated $updated action reference(s). Review changes with: git diff"
    fi

    if [[ $unresolved -ne 0 ]]; then
        echo "Error: could not resolve $unresolved action reference(s)." >&2
        exit 1
    fi
fi
