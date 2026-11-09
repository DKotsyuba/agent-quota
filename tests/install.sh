#!/bin/bash
# Isolated bootstrap checks; run with AQ_TEST_TMP pointing to a private directory.
set -Eeuo pipefail
trap 'printf "installer test failed at line %s\n" "$LINENO" >&2' ERR
fixture=${AQ_TEST_TMP:?}
installer=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/install.sh
real_wc=$(command -v wc)

if command -v sha256sum >/dev/null 2>&1; then
  real_hash=$(command -v sha256sum)
  real_hash_style=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  real_hash=$(command -v shasum)
  real_hash_style=shasum
else
  echo 'A SHA-256 utility is needed to run installer tests' >&2
  exit 1
fi

# Hash fixtures with the host's available SHA-256 utility.
digest() {
  local value
  if [[ $real_hash_style == shasum ]]; then
    value=$("$real_hash" -a 256 "$1")
  else
    value=$("$real_hash" "$1")
  fi
  printf '%s' "${value%% *}"
}

# Prepare a synthetic release and isolate its commands from the real network.
setup_case() {
  case_dir="$fixture/$1"
  test_os=$2 test_arch=$3 test_target=$4 test_hash=$5 test_auth=$6
  test_asset="agent-quota-$test_target"
  mkdir -p "$case_dir/commands" "$case_dir/assets" "$case_dir/tmp" "$case_dir/home"
  local tool
  for tool in awk chmod cp mkdir mktemp rm wc; do
    ln -s "$(command -v "$tool")" "$case_dir/commands/$tool"
  done

  cat > "$case_dir/commands/uname" <<'EOF'
#!/bin/bash
case "$1" in -s) printf '%s\n' "$TEST_OS";; -m) printf '%s\n' "$TEST_ARCH";; *) exit 1;; esac
EOF
  cat > "$case_dir/commands/curl" <<'EOF'
#!/bin/bash
set -euo pipefail
[[ $# == 18 && $1 == --disable && $2 == --fail && $3 == --silent && $4 == --show-error && $5 == --location ]]
[[ $6 == --proto && $7 == =https && $8 == --proto-redir && $9 == =https ]]
[[ ${10} == --connect-timeout && ${11} == 15 && ${12} == --max-time && ${13} == 300 ]]
[[ ${14} == --max-filesize && ${17} == --output ]]
prefix=https://github.com/$TEST_REPO/releases/download/v1.2.3/
[[ ${16} == "$prefix"* ]]
name=${16#"$prefix"}
case "$name:${15}" in SHA256SUMS:8192|release-manifest.json:16384|"$TEST_ASSET":536870912) ;; *) exit 1;; esac
printf '%s\n' "$name" >> "$TEST_ROOT/downloads"
cp "$TEST_ROOT/assets/$name" "${18}"
EOF
  cat > "$case_dir/commands/gh" <<'EOF'
#!/bin/bash
set -euo pipefail
if [[ $# == 4 && $1 == auth && $2 == status && $3 == --hostname && $4 == github.com ]]; then
  [[ $TEST_AUTH == yes ]]
  exit
fi
[[ ${GH_HOST:-} == github.com && $1 == release && $3 == v1.2.3 && $4 == --repo && $5 == "$TEST_REPO" ]]
if [[ $2 == view && $# == 9 && $6 == --json && $7 == assets && $8 == --jq ]]; then
  for name in SHA256SUMS release-manifest.json "$TEST_ASSET"; do
    if [[ ${9} == ".assets[] | select(.name == \"$name\") | .size" ]]; then
      if [[ ${TEST_OVERSIZE:-} == "$name" ]]; then
        printf '%s\n' 536870913
      else
        wc -c < "$TEST_ROOT/assets/$name" | awk '{print $1}'
      fi
      exit 0
    fi
  done
  exit 1
fi
[[ $2 == download && $# == 9 && $6 == --pattern && $8 == --output ]]
case $7 in SHA256SUMS|release-manifest.json|"$TEST_ASSET") ;; *) exit 1;; esac
printf '%s\n' "$7" >> "$TEST_ROOT/downloads"
cp "$TEST_ROOT/assets/$7" "$9"
EOF
  cat > "$case_dir/hash-mock" <<'EOF'
#!/bin/bash
set -euo pipefail
case ${0##*/} in
  shasum) [[ $# == 3 && $1 == -a && $2 == 256 ]]; shift 2;;
  sha256sum) [[ $# == 1 ]];;
  *) exit 1;;
esac
printf '%s\n' "${0##*/}" >> "$TEST_ROOT/hashes"
if [[ $REAL_HASH_STYLE == shasum ]]; then
  exec "$REAL_HASH" -a 256 "$1"
else
  exec "$REAL_HASH" "$1"
fi
EOF
  chmod 700 "$case_dir/commands/uname" "$case_dir/commands/curl" "$case_dir/commands/gh" "$case_dir/hash-mock"
  if [[ $test_hash != none ]]; then
    ln -s "$case_dir/hash-mock" "$case_dir/commands/$test_hash"
  fi
  cat > "$case_dir/assets/$test_asset" <<'EOF'
#!/bin/bash
set -euo pipefail
[[ $# == 7 && $1 == self-install && $2 == --bundle && $4 == --home && $6 == --bin-dir ]]
[[ $3 == "$TMPDIR"/agent-mcp-install.*/bundle && $5 == "$TEST_ROOT/private home" && $7 == "$TEST_ROOT/bin dir" ]]
[[ -z ${GH_TOKEN+x} && -z ${GITHUB_TOKEN+x} && -z ${GH_ENTERPRISE_TOKEN+x} && -z ${GITHUB_ENTERPRISE_TOKEN+x} ]]
[[ $(<"$TEST_ROOT/hashes") == "$TEST_HASH"$'\n'"$TEST_HASH" ]]
printf '%s\n' invoked > "$TEST_ROOT/executed"
EOF
  printf '%s\n' '{"fixture":true}' > "$case_dir/assets/release-manifest.json"
  printf '%s  %s\n%s  %s\n' "$(digest "$case_dir/assets/$test_asset")" "$test_asset" \
    "$(digest "$case_dir/assets/release-manifest.json")" release-manifest.json > "$case_dir/assets/SHA256SUMS"
}

# Run with an empty environment, synthetic credentials and only mocked clients.
run_install() {
  local repo_args=(--repo example/agent-quota)
  local expected_repo=example/agent-quota
  if [[ ${test_default_repo:-no} == yes ]]; then
    repo_args=()
    expected_repo=DKotsyuba/agent-quota
  fi
  env -i PATH="$case_dir/commands" HOME="$case_dir/home" TMPDIR="$case_dir/tmp" TMP="$case_dir/tmp" TEMP="$case_dir/tmp" \
    TEST_ROOT="$case_dir" TEST_OS="$test_os" TEST_ARCH="$test_arch" TEST_ASSET="$test_asset" TEST_AUTH="$test_auth" TEST_HASH="$test_hash" TEST_REPO="$expected_repo" \
    TEST_OVERSIZE="${test_oversize:-}" REAL_HASH="$real_hash" REAL_HASH_STYLE="$real_hash_style" REAL_WC="$real_wc" \
    GH_TOKEN=synthetic GITHUB_TOKEN=synthetic GH_ENTERPRISE_TOKEN=synthetic GITHUB_ENTERPRISE_TOKEN=synthetic \
    /bin/bash "$installer" --version 1.2.3 "${repo_args[@]}" \
    --home "$case_dir/private home" --bin-dir "$case_dir/bin dir" > "$case_dir/out" 2> "$case_dir/error"
}

# Check both files were verified before the synthetic candidate ran.
check_success() {
  run_install
  [[ $(<"$case_dir/executed") == invoked ]]
  [[ $(<"$case_dir/downloads") == $'SHA256SUMS\nrelease-manifest.json\n'"$test_asset" ]]
  [[ $(<"$case_dir/hashes") == "$test_hash"$'\n'"$test_hash" ]]
  [[ -d "$case_dir/bin dir" ]]
  [[ -z $(ls -A "$case_dir/tmp") ]]
}

# Check failure prevents installation and removes temporary files.
check_failure() {
  if run_install; then
    echo "Unexpected installer success: $case_dir" >&2
    exit 1
  fi
  [[ ! -e "$case_dir/executed" && ! -e "$case_dir/bin dir" ]]
  [[ -z $(ls -A "$case_dir/tmp") ]]
}

setup_case default_repo Linux x86_64 x86_64-unknown-linux-gnu sha256sum no
test_default_repo=yes
check_success
test_default_repo=no
setup_case linux Linux x86_64 x86_64-unknown-linux-gnu sha256sum no
check_success
setup_case macos Darwin arm64 aarch64-apple-darwin shasum no
check_success
setup_case authenticated Linux x86_64 x86_64-unknown-linux-gnu sha256sum yes
# BSD wc pads counts; the mocked GitHub API must still return a bare number.
rm "$case_dir/commands/wc"
cat > "$case_dir/commands/wc" <<'EOF'
#!/bin/bash
set -euo pipefail
count=$("$REAL_WC" "$@")
printf '%10s\n' "$count"
EOF
chmod 700 "$case_dir/commands/wc"
check_success

setup_case linux_arm Linux aarch64 x86_64-unknown-linux-gnu sha256sum no
check_failure
[[ $(<"$case_dir/error") == *'supports macOS arm64 and Linux x86_64 only'* && ! -e "$case_dir/downloads" ]]
setup_case macos_intel Darwin x86_64 aarch64-apple-darwin shasum no
check_failure
[[ $(<"$case_dir/error") == *'supports macOS arm64 and Linux x86_64 only'* && ! -e "$case_dir/downloads" ]]
setup_case no_hash Linux x86_64 x86_64-unknown-linux-gnu none no
check_failure
[[ $(<"$case_dir/error") == *'SHA-256 utility'* && ! -e "$case_dir/downloads" ]]

setup_case tampered_binary Linux x86_64 x86_64-unknown-linux-gnu sha256sum no
printf '%s\n' '# tampered' >> "$case_dir/assets/$test_asset"
check_failure
[[ $(<"$case_dir/error") == *'Checksum mismatch; no candidate executed'* ]]
setup_case tampered_manifest Darwin arm64 aarch64-apple-darwin shasum no
printf '%s\n' 'tampered' >> "$case_dir/assets/release-manifest.json"
check_failure
[[ $(<"$case_dir/error") == *'Checksum mismatch; no candidate executed'* ]]

setup_case duplicate_checksum Linux x86_64 x86_64-unknown-linux-gnu sha256sum no
printf '%s  %s\n' "$(digest "$case_dir/assets/$test_asset")" "$test_asset" >> "$case_dir/assets/SHA256SUMS"
check_failure
[[ ! -e "$case_dir/hashes" ]]
setup_case invalid_checksum Linux x86_64 x86_64-unknown-linux-gnu sha256sum no
printf '%s  %s\n' invalid "$test_asset" > "$case_dir/assets/SHA256SUMS"
check_failure
[[ $(<"$case_dir/error") == *'Invalid checksum entry'* ]]

setup_case excessive_size Linux x86_64 x86_64-unknown-linux-gnu sha256sum yes
test_oversize=$test_asset
check_failure
[[ $(<"$case_dir/error") == *'Invalid or excessive release asset size'* ]]
printf '%s\n' 'installer checks passed: Linux, macOS, authenticated download, unsupported platforms, missing hash, tampering, malformed checksums, size limits'
