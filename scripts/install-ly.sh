#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C
umask 077

repo="hyperlight-dev/lampyr"
workflow="$repo/.github/workflows/release.yml"
max_archive=$((1 << 30))
max_file=$((1 << 30))
max_total=$((4 << 30))
fail() {
    printf 'install-ly: %s\n' "$*" >&2
    exit 1
}

# A platform carries k8s/fiberd-tenant.yaml since fiberd-k8s-controller
# replaced the grant issuer, and k8s/grant-controller.yaml before that, so
# either marks a platform and an older release can still be selected.
platform_manifests=(k8s/fiberd-tenant.yaml k8s/grant-controller.yaml)
has_platform_manifest() {
    local manifest
    for manifest in "${platform_manifests[@]}"; do
        [[ ! -f "$1/$manifest" ]] || return 0
    done
    return 1
}

install_macos() {
    for tool in gh tar shasum mktemp install mv rm mkdir rmdir chmod stat od tr uname awk wc cat find sort uniq; do
        command -v "$tool" >/dev/null 2>&1 || fail "required command not found: $tool"
    done
    gh auth status >/dev/null 2>&1 || fail "authenticated gh access is required"

    (( $# <= 1 )) || fail "usage: install-ly.sh [release-tag]"
    local tag="${1-preview-release}" suffix source_ref object_type object_sha commit
    [[ "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]{0,127}$ ]] ||
        fail "invalid release tag"
    if [[ "$tag" == preview-release ]]; then
        suffix=preview
        source_ref=refs/heads/main
    else
        suffix="${tag#v}"
        source_ref="refs/tags/$tag"
        [[ -n "$suffix" ]] || fail "release tag has no asset suffix"
    fi

    [[ -n "${LAMPYR_BIN_DIR:-${HOME:-}}" ]] || fail "set HOME or LAMPYR_BIN_DIR"
    [[ -n "${LAMPYR_HOME:-${HOME:-}}" ]] || fail "set HOME or LAMPYR_HOME"
    mac_bin_dir="${LAMPYR_BIN_DIR:-$HOME/.local/bin}"
    mac_lampyr_home="${LAMPYR_HOME:-$HOME/.lampyr}"
    [[ "$mac_bin_dir" == /* && "$mac_lampyr_home" == /* ]] ||
        fail "installation destinations must be absolute"

    mac_work="$(mktemp -d "${TMPDIR:-/tmp}/.install-ly.XXXXXXXX")"
    mac_cli_extract="$mac_work/cli"
    mac_cli_name="lampyr-cli-aarch64-apple-darwin-$suffix.tar.gz"
    mac_platform_name="lampyr-platform-darwin-arm64-$suffix.tar.gz"
    mac_cli_archive="$mac_work/$mac_cli_name"
    mac_platform_archive="$mac_work/$mac_platform_name"
    mac_ly_destination="$mac_bin_dir/ly"
    mac_platform_stage=""
    mac_platform_destination=""
    mac_ly_stage=""
    mac_ly_backup=""
    mac_platform_backup=""
    mac_ly_moved=0
    mac_platform_moved=0
    mac_committed=0
    mac_bin_created=0
    mac_home_created=0
    mac_platforms_created=0
    [[ -e "$mac_bin_dir" ]] || mac_bin_created=1
    [[ -e "$mac_lampyr_home" ]] || mac_home_created=1
    [[ -e "$mac_lampyr_home/platforms" ]] || mac_platforms_created=1
    mkdir -p -- "$mac_bin_dir" "$mac_lampyr_home/platforms" "$mac_cli_extract"
    [[ -d "$mac_bin_dir" && ! -L "$mac_bin_dir" ]] || fail "unsafe binary directory"
    [[ -d "$mac_lampyr_home" && ! -L "$mac_lampyr_home" ]] ||
        fail "unsafe Lampyr home"
    [[ -d "$mac_lampyr_home/platforms" && ! -L "$mac_lampyr_home/platforms" ]] ||
        fail "unsafe platform directory"
    chmod 700 -- "$mac_lampyr_home" "$mac_lampyr_home/platforms"
    mac_platform_stage="$(mktemp -d "$mac_lampyr_home/platforms/.install.XXXXXXXX")"

    mac_remove_tree() {
        [[ -z "$1" ]] || rm -rf -- "$1"
    }

    cleanup_macos() {
        local status=$?
        trap - EXIT INT TERM
        if (( status != 0 && mac_committed == 0 )); then
            (( mac_ly_moved == 0 )) || rm -f -- "$mac_ly_destination"
            [[ -z "$mac_ly_backup" ]] ||
                mv -- "$mac_ly_backup" "$mac_ly_destination" || true
            mac_ly_backup=""
            if (( mac_platform_moved )); then
                mac_remove_tree "$mac_platform_destination"
            fi
            [[ -z "$mac_platform_backup" ]] ||
                mv -- "$mac_platform_backup" "$mac_platform_destination" || true
            mac_platform_backup=""
        fi
        [[ -z "$mac_ly_stage" ]] || rm -f -- "$mac_ly_stage"
        mac_remove_tree "$mac_platform_stage"
        mac_remove_tree "$mac_work"
        [[ -z "$mac_ly_backup" ]] || rm -f -- "$mac_ly_backup"
        mac_remove_tree "$mac_platform_backup"
        if (( status != 0 )); then
            (( mac_platforms_created == 0 )) ||
                rmdir "$mac_lampyr_home/platforms" 2>/dev/null || true
            (( mac_home_created == 0 )) || rmdir "$mac_lampyr_home" 2>/dev/null || true
            (( mac_bin_created == 0 )) || rmdir "$mac_bin_dir" 2>/dev/null || true
        fi
        exit "$status"
    }
    trap cleanup_macos EXIT
    trap 'exit 130' INT TERM

    read -r object_type object_sha < <(
        gh api "repos/$repo/git/ref/tags/$tag" --jq '.object.type + " " + .object.sha'
    )
    if [[ "$object_type" == tag ]]; then
        read -r object_type object_sha < <(
            gh api "repos/$repo/git/tags/$object_sha" --jq '.object.type + " " + .object.sha'
        )
    fi
    [[ "$object_type" == commit && "$object_sha" =~ ^[0-9a-f]{40}$ ]] ||
        fail "release tag does not resolve to a commit"
    commit="$(gh api "repos/$repo/commits/$object_sha" --jq .sha)"
    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "invalid release commit"

    gh release download "$tag" -R "$repo" -D "$mac_work" \
        -p "$mac_cli_name" -p "$mac_cli_name.sha256" \
        -p "$mac_platform_name" -p "$mac_platform_name.sha256"

    mac_verify_archive() {
        local archive="$1" name="${1##*/}" line expected actual
        [[ -f "$archive" && ! -L "$archive" &&
            -f "$archive.sha256" && ! -L "$archive.sha256" ]] ||
            fail "missing release asset: $name"
        line="$(cat "$archive.sha256")"
        [[ "$(wc -l <"$archive.sha256" | tr -d ' ')" == 1 &&
            "${line:0:64}" =~ ^[0-9a-f]{64}$ &&
            "${line:64:2}" == "  " && "${line:66}" == "$name" ]] ||
            fail "invalid checksum record: $name"
        expected="${line:0:64}"
        actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
        [[ "$actual" == "$expected" ]] || fail "checksum mismatch: $name"
        gh attestation verify "$archive" -R "$repo" \
            --signer-workflow "$workflow" \
            --source-ref "$source_ref" \
            --source-digest "$commit" >/dev/null
    }
    mac_verify_archive "$mac_cli_archive"
    mac_verify_archive "$mac_platform_archive"

    (( $(stat -f %z "$mac_cli_archive") <= max_archive )) ||
        fail "archive is too large"
    (( $(stat -f %z "$mac_platform_archive") <= max_archive )) ||
        fail "archive is too large"
    local names
    names="$(tar -tzf "$mac_cli_archive")"
    [[ "$names" == $'LICENSE\nly' ]] || fail "unexpected CLI archive content"
    tar -tvzf "$mac_cli_archive" |
        awk 'substr($1, 1, 1) != "-" { exit 1 }' ||
        fail "archive links or special files are not allowed"
    local license_size ly_size
    license_size="$(tar -xOzf "$mac_cli_archive" LICENSE | wc -c | tr -d ' ')"
    ly_size="$(tar -xOzf "$mac_cli_archive" ly | wc -c | tr -d ' ')"
    (( license_size <= max_file && ly_size <= max_file &&
        license_size + ly_size <= max_total )) ||
        fail "archive is too large"
    tar -xzf "$mac_cli_archive" -C "$mac_cli_extract"
    [[ -x "$mac_cli_extract/ly" && ! -L "$mac_cli_extract/ly" &&
        -f "$mac_cli_extract/LICENSE" && ! -L "$mac_cli_extract/LICENSE" ]] ||
        fail "invalid CLI payload"
    local header
    header="$(od -An -tx1 -N8 "$mac_cli_extract/ly" | tr -d ' \n')"
    [[ "$header" == cffaedfe0c000001 ]] ||
        fail "ly is not a macOS/arm64 executable"

    local platform_names platform_entries duplicates entry size total=0 files=0
    platform_names="$(tar -tzf "$mac_platform_archive")"
    [[ -n "$platform_names" ]] || fail "empty platform archive"
    platform_entries="$(printf '%s\n' "$platform_names" | wc -l | tr -d ' ')"
    (( platform_entries <= 100000 )) || fail "platform archive has too many entries"
    duplicates="$(printf '%s\n' "$platform_names" | sort | uniq -d)"
    [[ -z "$duplicates" ]] || fail "duplicate platform archive path"
    while IFS= read -r entry; do
        [[ -n "$entry" && "$entry" != /* && "$entry" != *//* &&
            "$entry" != *[!A-Za-z0-9._/@+=-]* ]] ||
            fail "unsafe platform archive path"
        case "/$entry/" in
            *"/../"* | *"/./"*) fail "platform archive path traversal" ;;
        esac
    done <<<"$platform_names"
    tar -tvzf "$mac_platform_archive" |
        awk 'substr($1, 1, 1) != "-" { exit 1 }' ||
        fail "platform archive links or special files are not allowed"
    tar -xzf "$mac_platform_archive" -C "$mac_platform_stage"
    while IFS= read -r -d '' entry; do
        size="$(stat -f %z "$entry")"
        (( size <= max_file && total + size <= max_total )) ||
            fail "platform archive is too large"
        (( total += size, files += 1 ))
    done < <(find "$mac_platform_stage" -type f -print0)
    (( files == platform_entries )) || fail "invalid platform archive entries"
    [[ -f "$mac_platform_stage/.lampyr-platform" &&
        -f "$mac_platform_stage/.lampyr-platform-commit" ]] ||
        fail "invalid platform payload"
    has_platform_manifest "$mac_platform_stage" || fail "invalid platform payload"
    local version platform_commit
    [[ "$(wc -l <"$mac_platform_stage/.lampyr-platform" | tr -d ' ')" == 1 ]] ||
        fail "invalid platform version"
    IFS= read -r version <"$mac_platform_stage/.lampyr-platform"
    [[ ${#version} -le 64 && "$version" != *..* && "$version" != "." &&
        "$version" =~ ^[A-Za-z0-9._-]+$ ]] || fail "invalid platform version"
    [[ "$tag" == preview-release || "$version" == "$suffix" ]] ||
        fail "platform version does not match release tag"
    [[ "$(wc -l <"$mac_platform_stage/.lampyr-platform-commit" | tr -d ' ')" == 1 ]] ||
        fail "invalid platform commit"
    IFS= read -r platform_commit <"$mac_platform_stage/.lampyr-platform-commit"
    [[ "$platform_commit" == "$commit" ]] ||
        fail "platform commit does not match release tag"
    mac_platform_destination="$mac_lampyr_home/platforms/$version"

    [[ ! -e "$mac_ly_destination" ||
        ( -f "$mac_ly_destination" && ! -L "$mac_ly_destination" ) ]] ||
        fail "unsafe existing ly destination"
    [[ ! -e "$mac_platform_destination" ||
        ( -d "$mac_platform_destination" && ! -L "$mac_platform_destination" ) ]] ||
        fail "unsafe existing platform destination"
    mac_ly_stage="$(mktemp "$mac_bin_dir/.ly.install.XXXXXXXX")"
    install -m 0755 "$mac_cli_extract/ly" "$mac_ly_stage"
    if [[ -e "$mac_ly_destination" ]]; then
        backup_reservation="$(mktemp "$mac_bin_dir/.ly.backup.XXXXXXXX")"
        rm "$backup_reservation"
        mac_ly_backup="$backup_reservation"
        mv -- "$mac_ly_destination" "$mac_ly_backup"
    fi
    if [[ -e "$mac_platform_destination" ]]; then
        mac_platform_backup="$(mktemp -d "$mac_lampyr_home/platforms/.backup.XXXXXXXX")"
        rmdir "$mac_platform_backup"
        mv -- "$mac_platform_destination" "$mac_platform_backup"
    fi
    mac_platform_moved=1
    mv -- "$mac_platform_stage" "$mac_platform_destination"
    mac_platform_stage=""
    mac_ly_moved=1
    mv -- "$mac_ly_stage" "$mac_ly_destination"
    mac_ly_stage=""
    mac_committed=1
    rm -f -- "$mac_ly_backup"
    mac_ly_backup=""
    mac_remove_tree "$mac_platform_backup"
    mac_platform_backup=""
    printf 'Installed ly to %s\n' "$mac_ly_destination"
    printf 'Installed Lampyr platform %s to %s\n' \
        "$version" "$mac_platform_destination"
}

system="$(uname -s)"
machine="$(uname -m)"
if [[ "$system" == Darwin && "$machine" == arm64 ]]; then
    install_macos "$@"
    exit 0
fi

for tool in gh tar sha256sum mktemp install mv rm mkdir rmdir chmod stat od tr uname; do
    command -v "$tool" >/dev/null 2>&1 || fail "required command not found: $tool"
done
[[ "$system" == Linux && "$machine" =~ ^(x86_64|amd64)$ ]] ||
    fail "supported platforms are Linux/amd64 and macOS/arm64"
[[ "$(tar --version)" == *"GNU tar"* ]] || fail "GNU tar is required"
gh auth status >/dev/null 2>&1 || fail "authenticated gh access is required"

(( $# <= 1 )) || fail "usage: install-ly.sh [release-tag]"
tag="${1-preview-release}"
[[ "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]{0,127}$ ]] ||
    fail "invalid release tag"
if [[ "$tag" == preview-release ]]; then
    suffix=preview
    source_ref=refs/heads/main
else
    suffix="${tag#v}"
    source_ref="refs/tags/$tag"
    [[ -n "$suffix" ]] || fail "release tag has no asset suffix"
fi

[[ -n "${LAMPYR_BIN_DIR:-${HOME:-}}" ]] || fail "set HOME or LAMPYR_BIN_DIR"
[[ -n "${LAMPYR_HOME:-${HOME:-}}" ]] || fail "set HOME or LAMPYR_HOME"
bin_dir="${LAMPYR_BIN_DIR:-$HOME/.local/bin}"
lampyr_home="${LAMPYR_HOME:-$HOME/.lampyr}"
[[ "$bin_dir" == /* && "$lampyr_home" == /* ]] ||
    fail "installation destinations must be absolute paths"

bin_created=0 home_created=0 platforms_created=0
[[ -e "$bin_dir" ]] || bin_created=1
[[ -e "$lampyr_home" ]] || home_created=1
[[ -e "$lampyr_home/platforms" ]] || platforms_created=1
mkdir -p -- "$bin_dir" "$lampyr_home/platforms"
[[ -d "$bin_dir" && ! -L "$bin_dir" ]] || fail "unsafe binary directory"
[[ -d "$lampyr_home" && ! -L "$lampyr_home" ]] || fail "unsafe Lampyr home"
[[ -d "$lampyr_home/platforms" && ! -L "$lampyr_home/platforms" ]] ||
    fail "unsafe platform directory"
chmod 700 -- "$lampyr_home" "$lampyr_home/platforms"

work="$(mktemp -d "$lampyr_home/.install-ly.XXXXXXXX")" cli_extract="$work/cli"
platform_stage="$(mktemp -d "$lampyr_home/platforms/.install.XXXXXXXX")"
ly_stage="" ly_backup="" platform_backup=""
ly_destination="$bin_dir/ly"
platform_destination=""
ly_moved=0 platform_moved=0 committed=0
mkdir "$cli_extract"

remove_tree() {
    [[ -z "$1" ]] || rm -rf --one-file-system -- "$1"
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if (( status != 0 && committed == 0 )); then
        (( ly_moved == 0 )) || rm -f -- "$ly_destination"
        [[ -z "$ly_backup" ]] || mv -T -- "$ly_backup" "$ly_destination" || true
        ly_backup=""
        if (( platform_moved )); then
            remove_tree "$platform_destination"
        fi
        [[ -z "$platform_backup" ]] ||
            mv -T -- "$platform_backup" "$platform_destination" || true
        platform_backup=""
    fi
    [[ -z "$ly_stage" ]] || rm -f -- "$ly_stage"
    remove_tree "$platform_stage"
    remove_tree "$work"
    [[ -z "$ly_backup" ]] || rm -f -- "$ly_backup"
    remove_tree "$platform_backup"
    if (( status != 0 )); then
        (( platforms_created == 0 )) || rmdir "$lampyr_home/platforms" 2>/dev/null || true
        (( home_created == 0 )) || rmdir "$lampyr_home" 2>/dev/null || true
        (( bin_created == 0 )) || rmdir "$bin_dir" 2>/dev/null || true
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

read -r object_type object_sha < <(
    gh api "repos/$repo/git/ref/tags/$tag" --jq '.object.type + " " + .object.sha'
)
if [[ "$object_type" == tag ]]; then
    read -r object_type object_sha < <(
        gh api "repos/$repo/git/tags/$object_sha" --jq '.object.type + " " + .object.sha'
    )
fi
[[ "$object_type" == commit && "$object_sha" =~ ^[0-9a-f]{40}$ ]] ||
    fail "release tag does not resolve to a commit"
commit="$(gh api "repos/$repo/commits/$object_sha" --jq .sha)"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "invalid release commit"

cli_name="lampyr-cli-linux-amd64-$suffix.tar.gz"
platform_name="lampyr-platform-linux-amd64-$suffix.tar.gz"
gh release download "$tag" -R "$repo" -D "$work" -p "$cli_name" -p "$cli_name.sha256" \
    -p "$platform_name" -p "$platform_name.sha256"
cli_archive="$work/$cli_name"
platform_archive="$work/$platform_name"

verify_checksum() {
    local archive="$1" name="${1##*/}" line
    local -a lines
    [[ -f "$archive" && ! -L "$archive" && -f "$archive.sha256" &&
        ! -L "$archive.sha256" ]] || fail "missing release asset: $name"
    mapfile -t lines <"$archive.sha256"
    (( ${#lines[@]} == 1 )) || fail "invalid checksum file: $name"
    line="${lines[0]}"
    [[ "${line:0:64}" =~ ^[0-9a-f]{64}$ &&
        "${line:64:2}" == "  " && "${line:66}" == "$name" ]] ||
        fail "invalid checksum record: $name"
    (cd "$work" && sha256sum --check --strict "$name.sha256" >/dev/null)
}

verify_checksum "$cli_archive"
verify_checksum "$platform_archive"
for archive in "$cli_archive" "$platform_archive"; do
    gh attestation verify "$archive" -R "$repo" \
        --signer-workflow "$workflow" \
        --source-ref "$source_ref" \
        --source-digest "$commit" \
        --deny-self-hosted-runners >/dev/null
done

preflight() {
    local archive="$1" kind="$2" name mode size type path manifest i total=0 files=0
    local found_ly=0 found_license=0 found_marker=0 found_commit=0 found_manifest=0
    local -a names metadata
    local -A seen
    (( $(stat -c %s "$archive") <= max_archive )) || fail "archive is too large"
    tar -Ptzf "$archive" --quoting-style=escape >"$work/$kind.names"
    tar -Ptvzf "$archive" --numeric-owner --full-time \
        --quoting-style=escape >"$work/$kind.meta"
    mapfile -t names <"$work/$kind.names"
    mapfile -t metadata <"$work/$kind.meta"
    (( ${#names[@]} > 0 && ${#names[@]} == ${#metadata[@]} &&
        ${#names[@]} <= 100000 )) || fail "invalid archive entry count"

    for (( i = 0; i < ${#names[@]}; i++ )); do
        name="${names[i]}"
        read -r mode _ size _ <<<"${metadata[i]}"
        type="${mode:0:1}"
        [[ -n "$name" && "$name" != /* && "$name" != *//* &&
            "$name" != *[!A-Za-z0-9._/@+=-]* ]] ||
            fail "unsafe archive path"
        path="${name%/}"
        case "/$path/" in
            *"/../"* | *"/./"*) fail "archive path traversal" ;;
        esac
        [[ -n "$path" && -z "${seen[$path]+x}" ]] || fail "duplicate archive path"
        seen["$path"]=1
        case "$type" in
            -)
                [[ "$name" != */ && "$size" =~ ^[0-9]+$ ]] ||
                    fail "invalid regular file"
                (( size <= max_file && total + size <= max_total )) ||
                    fail "archive is too large"
                (( total += size, files += 1 ))
                ;;
            d) ;;
            l | h) fail "archive links are not allowed" ;;
            *) fail "archive special files are not allowed" ;;
        esac
        if [[ "$kind" == cli ]]; then
            [[ "$type" == - ]] || fail "unexpected CLI archive content"
            case "$path" in
                ly) found_ly=1 ;;
                LICENSE) found_license=1 ;;
                *) fail "unexpected CLI archive content" ;;
            esac
        else
            [[ "$path" != .lampyr-platform || "$type" == - ]] ||
                fail "invalid platform marker"
            [[ "$path" != .lampyr-platform-commit || "$type" == - ]] ||
                fail "invalid platform commit marker"
            for manifest in "${platform_manifests[@]}"; do
                [[ "$path" != "$manifest" || "$type" == - ]] ||
                    fail "invalid platform manifest"
                [[ "$path" != "$manifest" ]] || found_manifest=1
            done
            [[ "$path" != .lampyr-platform ]] || found_marker=1
            [[ "$path" != .lampyr-platform-commit ]] || found_commit=1
        fi
    done
    if [[ "$kind" == cli ]]; then
        (( files == 2 && found_ly && found_license )) || fail "invalid CLI archive"
    else
        (( found_marker && found_commit && found_manifest )) ||
            fail "incomplete platform archive"
    fi
}

preflight "$cli_archive" cli
preflight "$platform_archive" platform
tar -xzf "$cli_archive" -C "$cli_extract" --no-same-owner --no-same-permissions
tar -xzf "$platform_archive" -C "$platform_stage" --no-same-owner --no-same-permissions

[[ -x "$cli_extract/ly" && -f "$cli_extract/LICENSE" ]] || fail "invalid CLI payload"
elf="$(od -An -tx1 -N20 "$cli_extract/ly" | tr -d ' \n')"
[[ "${elf:0:12}" == 7f454c460201 && "${elf:36:4}" == 3e00 ]] ||
    fail "ly is not a Linux/amd64 ELF"
[[ -f "$platform_stage/.lampyr-platform" &&
    -f "$platform_stage/.lampyr-platform-commit" ]] || fail "invalid platform payload"
has_platform_manifest "$platform_stage" || fail "invalid platform payload"
mapfile -t versions <"$platform_stage/.lampyr-platform"
(( ${#versions[@]} == 1 )) || fail "invalid platform version"
version="${versions[0]}"
[[ ${#version} -le 64 && "$version" != *..* && "$version" != "." &&
    "$version" =~ ^[A-Za-z0-9._-]+$ ]] || fail "invalid platform version"
[[ "$tag" == preview-release || "$version" == "$suffix" ]] ||
    fail "platform version does not match release tag"
mapfile -t platform_commits <"$platform_stage/.lampyr-platform-commit"
(( ${#platform_commits[@]} == 1 )) ||
    fail "invalid platform commit"
[[ "${platform_commits[0]}" == "$commit" ]] ||
    fail "platform commit does not match release tag"
platform_destination="$lampyr_home/platforms/$version"

[[ ! -e "$ly_destination" || ( -f "$ly_destination" && ! -L "$ly_destination" ) ]] ||
    fail "unsafe existing ly destination"
[[ ! -e "$platform_destination" ||
    ( -d "$platform_destination" && ! -L "$platform_destination" ) ]] ||
    fail "unsafe existing platform destination"
ly_stage="$(mktemp "$bin_dir/.ly.install.XXXXXXXX")"
install -m 0755 "$cli_extract/ly" "$ly_stage"

if [[ -e "$ly_destination" ]]; then
    ly_backup="$(mktemp "$bin_dir/.ly.backup.XXXXXXXX")"
    rm "$ly_backup"
    mv -T "$ly_destination" "$ly_backup"
fi
if [[ -e "$platform_destination" ]]; then
    platform_backup="$(mktemp -d "$lampyr_home/platforms/.backup.XXXXXXXX")"
    rmdir "$platform_backup"
    mv -T "$platform_destination" "$platform_backup"
fi
mv -T "$platform_stage" "$platform_destination"
platform_stage=""
platform_moved=1
mv -T "$ly_stage" "$ly_destination"
ly_stage=""
ly_moved=1
committed=1
rm -f -- "$ly_backup"
ly_backup=""
remove_tree "$platform_backup"
platform_backup=""

printf 'Installed ly to %s\n' "$ly_destination"
printf 'Installed Lampyr platform %s to %s\n' "$version" "$platform_destination"
