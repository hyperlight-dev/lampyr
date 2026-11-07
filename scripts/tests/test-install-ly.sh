#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
installer="$root/scripts/install-ly.sh"
test_root="$root/build/test-install-ly"
releases="$test_root/releases"
fake_bin="$test_root/bin"
log="$test_root/gh.log"
commit=0123456789abcdef0123456789abcdef01234567
token=secret-token-must-not-leak
trap 'rm -rf "$test_root"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
contains() { [[ "$(<"$1")" == *"$2"* ]] || fail "missing '$2' in $1"; }

mkdir -p "$releases" "$fake_bin"
cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_LOG"
case "${1:-} ${2:-}" in
    "auth status") [[ "${FAKE_AUTH_FAIL:-0}" == 0 ]] ;;
    "api "*)
        case "$2" in
            */git/ref/tags/*) printf 'commit %s\n' "$FAKE_COMMIT" ;;
            */commits/*) printf '%s\n' "$FAKE_COMMIT" ;;
            *) exit 2 ;;
        esac
        ;;
    "release download")
        tag="$3"; shift 3; patterns=()
        while (( $# )); do
            case "$1" in
                -R) shift 2 ;;
                -D) destination="$2"; shift 2 ;;
                -p) patterns+=("$2"); shift 2 ;;
                *) exit 2 ;;
            esac
        done
        for file in "${patterns[@]}"; do
            cp "$FAKE_RELEASES/$tag/$file" "$destination/$file"
        done
        ;;
    "attestation verify")
        [[ "${3##*/}" != "${FAKE_ATTEST_FAIL:-none}" ]]
        ;;
    *) exit 2 ;;
esac
EOF
chmod +x "$fake_bin/gh"
cat >"$fake_bin/uname" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
    -s) printf '%s\n' "${FAKE_UNAME_S:-Linux}" ;;
    -m) printf '%s\n' "${FAKE_UNAME_M:-x86_64}" ;;
    *) exit 2 ;;
esac
EOF
cat >"$fake_bin/stat" <<'EOF'
#!/usr/bin/env bash
if [[ "${FAKE_UNAME_S:-Linux}" == Darwin && "${1:-}" == -f ]]; then
    case "${2:-}" in
        %z) wc -c <"$3" | tr -d ' ' ;;
        %Lp) /usr/bin/stat -c '%a' "$3" ;;
        *) exit 2 ;;
    esac
else
    /usr/bin/stat "$@"
fi
EOF
cat >"$fake_bin/shasum" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == -a && "$2" == 256 ]] || exit 2
shift 2
sha256sum "$@"
EOF
cat >"$fake_bin/mv" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
/bin/mv "$@"
if [[ "${FAKE_MV_INTERRUPT:-0}" == 1 &&
    "${1:-}" == -- && (
        ( "${FAKE_MV_INTERRUPT_TARGET:-cli}" == cli &&
          "${2:-}" == */.ly.install.* && "${3:-}" == */bin/ly ) ||
        ( "${FAKE_MV_INTERRUPT_TARGET:-cli}" == platform &&
          "${2:-}" == */platforms/.install.* &&
          "${3:-}" == */platforms/* )
    ) ]]; then
    kill -TERM "$PPID"
fi
EOF
chmod +x "$fake_bin/uname" "$fake_bin/stat" "$fake_bin/shasum" "$fake_bin/mv"

# A release's platform marks itself with k8s/fiberd-tenant.yaml, or, from
# before fiberd-k8s-controller, k8s/grant-controller.yaml; $4 picks which.
make_release() {
    local tag="$1" suffix="$2" version="$3" dir="$releases/$1" payload="$test_root/payload-$1"
    local manifest="${4:-k8s/fiberd-tenant.yaml}"
    local cli="lampyr-cli-linux-amd64-$suffix.tar.gz"
    local platform="lampyr-platform-linux-amd64-$suffix.tar.gz"
    rm -rf "$dir" "$payload"
    mkdir -p "$dir" "$payload/cli" "$payload/platform/k8s"
    cp /bin/true "$payload/cli/ly"
    printf license >"$payload/cli/LICENSE"
    printf '%s\n' "$version" >"$payload/platform/.lampyr-platform"
    printf '%s\n' "$commit" >"$payload/platform/.lampyr-platform-commit"
    printf manifest >"$payload/platform/$manifest"
    printf new >"$payload/platform/content"
    tar -czf "$dir/$cli" -C "$payload/cli" ly LICENSE
    tar -czf "$dir/$platform" -C "$payload/platform" \
        .lampyr-platform .lampyr-platform-commit "$manifest" content
    (cd "$dir" && sha256sum "$cli" >"$cli.sha256" &&
        sha256sum "$platform" >"$platform.sha256")
}

make_macos_release() {
    local tag="$1" suffix="$2" version="$3" dir="$releases/$1" payload="$test_root/payload-$1"
    local manifest="${4:-k8s/fiberd-tenant.yaml}"
    local cli="lampyr-cli-aarch64-apple-darwin-$suffix.tar.gz"
    local platform="lampyr-platform-darwin-arm64-$suffix.tar.gz"
    rm -rf "$dir" "$payload"
    mkdir -p "$dir" "$payload/cli" "$payload/platform/k8s"
    printf '\317\372\355\376\014\000\000\001macos-cli' >"$payload/cli/ly"
    chmod +x "$payload/cli/ly"
    printf license >"$payload/cli/LICENSE"
    printf '%s\n' "$version" >"$payload/platform/.lampyr-platform"
    printf '%s\n' "$commit" >"$payload/platform/.lampyr-platform-commit"
    printf manifest >"$payload/platform/$manifest"
    printf macos >"$payload/platform/content"
    tar -czf "$dir/$cli" -C "$payload/cli" LICENSE ly
    tar -czf "$dir/$platform" -C "$payload/platform" \
        .lampyr-platform .lampyr-platform-commit "$manifest" content
    (cd "$dir" && sha256sum "$cli" >"$cli.sha256" &&
        sha256sum "$platform" >"$platform.sha256")
}

old_install() {
    mkdir -p "$1/bin" "$1/home/platforms/$2"
    chmod 0775 "$1/home" "$1/home/platforms"
    printf old >"$1/bin/ly"
    chmod +x "$1/bin/ly"
    printf '%s\n' "$2" >"$1/home/platforms/$2/.lampyr-platform"
    printf stale >"$1/home/platforms/$2/stale"
}

assert_private_directory() {
    local mode
    if [[ "$(uname -s)" == Darwin ]]; then
        mode="$(stat -f '%Lp' "$1")"
    else
        mode="$(stat -c '%a' "$1")"
    fi
    [[ "$mode" == 700 ]] || fail "$1 has mode $mode, expected 700"
}

old_cli_install() {
    mkdir -p "$1/bin"
    printf old >"$1/bin/ly"
    chmod +x "$1/bin/ly"
}

run() {
    local name="$1" destination="$2"; shift 2
    output="$test_root/$name.output"
    : >"$log"
    stdout="$output"
    [[ "${TEST_REPORT_FAIL:-0}" == 0 ]] || stdout=/dev/full
    set +e
    mkdir -p "$destination/tmp"
    env PATH="$fake_bin:$PATH" HOME="$destination/user" TMPDIR="$destination/tmp" \
        LAMPYR_BIN_DIR="$destination/bin" LAMPYR_HOME="$destination/home" \
        GH_TOKEN="$token" FAKE_LOG="$log" FAKE_RELEASES="$releases" \
        FAKE_COMMIT="$commit" FAKE_UNAME_S="${FAKE_UNAME_S:-Linux}" \
        FAKE_UNAME_M="${FAKE_UNAME_M:-x86_64}" \
        FAKE_MV_INTERRUPT="${FAKE_MV_INTERRUPT:-0}" \
        FAKE_MV_INTERRUPT_TARGET="${FAKE_MV_INTERRUPT_TARGET:-cli}" \
        "$installer" "$@" >"$stdout" 2>>"$output"
    result=$?
    set -e
    [[ "$(<"$output")" != *"$token"* ]] || fail "token leaked"
    return "$result"
}

make_release preview-release preview 0.1.0-preview.9
destination="$test_root/preview"
old_install "$destination" 0.1.0-preview.9
run preview "$destination" || fail "$(<"$output")"
cmp "$destination/bin/ly" /bin/true || fail "ly was not replaced"
[[ -f "$destination/home/platforms/0.1.0-preview.9/content" &&
    ! -e "$destination/home/platforms/0.1.0-preview.9/stale" ]] ||
    fail "platform was not replaced"
assert_private_directory "$destination/home"
assert_private_directory "$destination/home/platforms"
contains "$log" "lampyr-cli-linux-amd64-preview.tar.gz"
contains "$log" "--signer-workflow hyperlight-dev/lampyr/.github/workflows/release.yml"
contains "$log" "--source-digest $commit"
contains "$log" "--source-ref refs/heads/main"
contains "$log" "--deny-self-hosted-runners"
[[ "$(grep -c '^attestation verify ' "$log")" == 2 ]] ||
    fail "both attestations were not checked"

make_release v1.2.3 1.2.3 1.2.3
destination="$test_root/versioned"
run versioned "$destination" v1.2.3 || fail "$(<"$output")"
[[ -x "$destination/bin/ly" &&
    -f "$destination/home/platforms/1.2.3/content" ]] ||
    fail "versioned install failed"
contains "$log" "lampyr-platform-linux-amd64-1.2.3.tar.gz"
contains "$log" "--source-ref refs/tags/v1.2.3"

make_release v0.1.0 0.1.0 0.1.0 k8s/grant-controller.yaml
destination="$test_root/pre-controller"
run pre-controller "$destination" v0.1.0 || fail "$(<"$output")"
[[ -x "$destination/bin/ly" &&
    -f "$destination/home/platforms/0.1.0/k8s/grant-controller.yaml" ]] ||
    fail "a release from before fiberd-k8s-controller was refused"

make_macos_release preview-release preview 0.1.0-preview.9
destination="$test_root/macos-preview"
export FAKE_UNAME_S=Darwin FAKE_UNAME_M=arm64
run macos-preview "$destination" || fail "$(<"$output")"
[[ -x "$destination/bin/ly" ]] || fail "macOS ly was not installed"
header="$(od -An -tx1 -N8 "$destination/bin/ly" | tr -d ' \n')"
[[ "$header" == cffaedfe0c000001 ]] || fail "wrong macOS binary installed"
[[ -f "$destination/home/platforms/0.1.0-preview.9/content" ]] ||
    fail "macOS platform was not installed"
assert_private_directory "$destination/home"
assert_private_directory "$destination/home/platforms"
contains "$log" "lampyr-cli-aarch64-apple-darwin-preview.tar.gz"
contains "$log" "lampyr-platform-darwin-arm64-preview.tar.gz"
[[ "$(<"$log")" != *"lampyr-platform-linux-amd64"* ]] ||
    fail "macOS install downloaded the Linux platform"
[[ "$(grep -c '^attestation verify ' "$log")" == 2 ]] ||
    fail "macOS CLI and platform attestations were not checked"
[[ "$(<"$log")" != *"--deny-self-hosted-runners"* ]] ||
    fail "macOS self-hosted provenance was incorrectly rejected"

make_macos_release v0.1.0 0.1.0 0.1.0 k8s/grant-controller.yaml
destination="$test_root/macos-pre-controller"
run macos-pre-controller "$destination" v0.1.0 || fail "$(<"$output")"
[[ -x "$destination/bin/ly" &&
    -f "$destination/home/platforms/0.1.0/k8s/grant-controller.yaml" ]] ||
    fail "a macOS release from before fiberd-k8s-controller was refused"

make_macos_release macos-no-manifest macos-no-manifest macos-no-manifest
payload="$test_root/payload-macos-no-manifest/platform"
archive="lampyr-platform-darwin-arm64-macos-no-manifest.tar.gz"
tar -czf "$releases/macos-no-manifest/$archive" -C "$payload" \
    .lampyr-platform .lampyr-platform-commit content
(cd "$releases/macos-no-manifest" && sha256sum "$archive" >"$archive.sha256")
if run macos-no-manifest "$test_root/macos-no-manifest" macos-no-manifest; then
    fail "a macOS platform without a manifest was accepted"
fi
contains "$output" "invalid platform payload"

make_macos_release macos-bad macos-bad macos-bad
printf not-macho >"$test_root/payload-macos-bad/cli/ly"
chmod +x "$test_root/payload-macos-bad/cli/ly"
tar -czf "$releases/macos-bad/lampyr-cli-aarch64-apple-darwin-macos-bad.tar.gz" \
    -C "$test_root/payload-macos-bad/cli" LICENSE ly
(cd "$releases/macos-bad" &&
    sha256sum lampyr-cli-aarch64-apple-darwin-macos-bad.tar.gz \
        >lampyr-cli-aarch64-apple-darwin-macos-bad.tar.gz.sha256)
destination="$test_root/macos-bad"
old_cli_install "$destination"
if run macos-bad "$destination" macos-bad; then
    fail "non-Mach-O macOS CLI was accepted"
fi
[[ "$(<"$destination/bin/ly")" == old ]] ||
    fail "failed macOS install did not restore the previous CLI"

make_macos_release macos-interrupt macos-interrupt macos-interrupt
destination="$test_root/macos-interrupt"
export FAKE_MV_INTERRUPT=1
if run macos-interrupt "$destination" macos-interrupt; then
    fail "interrupted macOS install reported success"
fi
[[ ! -e "$destination/bin/ly" ]] ||
    fail "interrupted first macOS install left an uncommitted CLI"
[[ ! -e "$destination/home/platforms/macos-interrupt" ]] ||
    fail "interrupted first macOS install left an uncommitted platform"
unset FAKE_MV_INTERRUPT

make_macos_release macos-platform-interrupt macos-platform-interrupt macos-platform-interrupt
destination="$test_root/macos-platform-interrupt"
old_install "$destination" macos-platform-interrupt
export FAKE_MV_INTERRUPT=1 FAKE_MV_INTERRUPT_TARGET=platform
if run macos-platform-interrupt "$destination" macos-platform-interrupt; then
    fail "interrupted macOS platform install reported success"
fi
[[ "$(<"$destination/bin/ly")" == old ]] ||
    fail "interrupted macOS platform install replaced the CLI"
[[ -f "$destination/home/platforms/macos-platform-interrupt/stale" &&
    ! -f "$destination/home/platforms/macos-platform-interrupt/content" ]] ||
    fail "interrupted macOS platform install did not restore the previous platform"
unset FAKE_MV_INTERRUPT FAKE_MV_INTERRUPT_TARGET
unset FAKE_UNAME_S FAKE_UNAME_M

export FAKE_UNAME_S=Darwin FAKE_UNAME_M=x86_64
if run unsupported "$test_root/unsupported"; then
    fail "unsupported macOS architecture was accepted"
fi
contains "$output" "supported platforms are Linux/amd64 and macOS/arm64"
unset FAKE_UNAME_S FAKE_UNAME_M

make_release report-fail report-fail report-fail
destination="$test_root/report-fail"
old_install "$destination" report-fail
export TEST_REPORT_FAIL=1
if run report-fail "$destination" report-fail; then fail "reporting failure was ignored"; fi
unset TEST_REPORT_FAIL
cmp "$destination/bin/ly" /bin/true || fail "reporting failure rolled back ly"
[[ -f "$destination/home/platforms/report-fail/content" &&
    ! -e "$destination/home/platforms/report-fail/stale" ]] ||
    fail "reporting failure rolled back platform"

export FAKE_AUTH_FAIL=1
if run auth "$test_root/auth"; then fail "missing gh auth was accepted"; fi
unset FAKE_AUTH_FAIL

make_release bad-sum bad-sum bad-sum
sum="$releases/bad-sum/lampyr-platform-linux-amd64-bad-sum.tar.gz.sha256"
printf '%064d  lampyr-platform-linux-amd64-bad-sum.tar.gz\n' 0 >"$sum"
if run checksum "$test_root/checksum" bad-sum; then fail "bad checksum was accepted"; fi

make_release bad-attest bad-attest bad-attest
export FAKE_ATTEST_FAIL=lampyr-platform-linux-amd64-bad-attest.tar.gz
if run attestation "$test_root/attestation" bad-attest; then
    fail "bad attestation was accepted"
fi
unset FAKE_ATTEST_FAIL

make_release bad-commit bad-commit bad-commit
payload="$test_root/payload-bad-commit/platform"
archive="lampyr-platform-linux-amd64-bad-commit.tar.gz"
printf '%040d\n' 0 >"$payload/.lampyr-platform-commit"
tar -czf "$releases/bad-commit/$archive" -C "$payload" \
    .lampyr-platform .lampyr-platform-commit k8s/fiberd-tenant.yaml content
(cd "$releases/bad-commit" && sha256sum "$archive" >"$archive.sha256")
if run commit "$test_root/bad-commit-destination" bad-commit; then
    fail "mismatched platform commit was accepted"
fi

# A platform needs one of the manifests, and as a regular file.
make_release no-manifest no-manifest no-manifest
payload="$test_root/payload-no-manifest/platform"
archive="lampyr-platform-linux-amd64-no-manifest.tar.gz"
tar -czf "$releases/no-manifest/$archive" -C "$payload" \
    .lampyr-platform .lampyr-platform-commit content
(cd "$releases/no-manifest" && sha256sum "$archive" >"$archive.sha256")
if run no-manifest "$test_root/no-manifest-destination" no-manifest; then
    fail "a platform without a manifest was accepted"
fi
contains "$output" "incomplete platform archive"
for manifest in k8s/fiberd-tenant.yaml k8s/grant-controller.yaml; do
    tag="dir-${manifest#k8s/}"; tag="${tag%.yaml}"
    make_release "$tag" "$tag" "$tag"
    payload="$test_root/payload-$tag/platform"
    archive="lampyr-platform-linux-amd64-$tag.tar.gz"
    rm -f "$payload/k8s/fiberd-tenant.yaml"
    mkdir -p "$payload/$manifest"
    printf manifest >"$payload/$manifest/inner"
    tar -czf "$releases/$tag/$archive" -C "$payload" \
        .lampyr-platform .lampyr-platform-commit "$manifest" content
    (cd "$releases/$tag" && sha256sum "$archive" >"$archive.sha256")
    if run "$tag" "$test_root/$tag-destination" "$tag"; then
        fail "a platform whose $manifest is a directory was accepted"
    fi
    contains "$output" "invalid platform manifest"
done

bad_archive() {
    local kind="$1" tag="unsafe-$1" dir payload archive
    make_release "$tag" "$tag" "$tag"
    dir="$releases/$tag"; payload="$test_root/$tag"; archive="lampyr-cli-linux-amd64-$tag.tar.gz"
    mkdir -p "$payload"
    cp /bin/true "$payload/ly"; printf license >"$payload/LICENSE"
    case "$kind" in
        traversal)
            printf bad >"$payload/bad"
            tar -czf "$dir/$archive" --transform='s|^bad$|../bad|' -C "$payload" bad
            ;;
        link)
            ln -s ly "$payload/alias"
            tar -czf "$dir/$archive" -C "$payload" ly LICENSE alias
            ;;
        duplicate) tar -czf "$dir/$archive" -C "$payload" ly LICENSE ly ;;
        extra)
            printf bad >"$payload/extra"
            tar -czf "$dir/$archive" -C "$payload" ly LICENSE extra
            ;;
    esac
    (cd "$dir" && sha256sum "$archive" >"$archive.sha256")
    if run "$tag" "$test_root/$tag-destination" "$tag"; then
        fail "unsafe $kind archive was accepted"
    fi
}
for kind in traversal link duplicate extra; do bad_archive "$kind"; done

[[ -z "$(find "$test_root" -name '.install-ly.*' -o -name '.install.*' \
    -o -name '.ly.install.*' -o -name '.ly.backup.*' -o -name '.backup.*')" ]] ||
    fail "temporary state was left behind"
printf 'install-ly tests passed\n'
