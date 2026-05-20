#!/usr/bin/env bash
# Verify the public Beam APT repository without installing the package.
#
# Usage:
#   scripts/verify-apt-repo.sh [expected-version]
#
# The repository URL can be overridden for drills:
#   BEAM_APT_REPO_URL=https://raw.githubusercontent.com/frecar/beam/gh-pages scripts/verify-apt-repo.sh 0.3.23
set -euo pipefail

REPO_URL="${BEAM_APT_REPO_URL:-https://raw.githubusercontent.com/frecar/beam/gh-pages}"
EXPECTED_VERSION="${1:-${BEAM_EXPECTED_VERSION:-}}"

TMPDIR="$(mktemp -d)"
cleanup() {
  rm -rf "${TMPDIR}"
}
trap cleanup EXIT

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "ERROR: missing required command: $1" >&2
    exit 1
  fi
}

fetch() {
  local path="$1"
  local output="$2"
  curl -fsSL --retry 3 --connect-timeout 10 "${REPO_URL}/${path}" -o "${output}"
}

release_sha256_for() {
  local path="$1"
  awk -v path="${path}" '
    $1 == "SHA256:" { in_sha256 = 1; next }
    /^[A-Z0-9]+:/ && $1 != "SHA256:" { in_sha256 = 0 }
    in_sha256 && $3 == path { print $1; exit }
  ' "${TMPDIR}/Release"
}

package_field() {
  local field="$1"
  local file="$2"
  awk -v field="${field}:" '
    $1 == "Package:" && $2 == "beam" { in_pkg = 1; next }
    in_pkg && $1 == field { print $2; exit }
    in_pkg && NF == 0 { in_pkg = 0 }
  ' "${file}"
}

verify_sha256() {
  local path="$1"
  local file="$2"
  local expected="$3"
  local actual
  actual="$(sha256sum "${file}" | awk '{ print $1 }')"
  if [[ -z "${expected}" ]]; then
    echo "ERROR: missing SHA256 entry for ${path}" >&2
    exit 1
  fi
  if [[ "${actual}" != "${expected}" ]]; then
    echo "ERROR: SHA256 mismatch for ${path}" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    exit 1
  fi
}

verify_apt_candidate() {
  if ! command -v apt-get >/dev/null 2>&1 || ! command -v apt-cache >/dev/null 2>&1; then
    echo "WARN: apt-get/apt-cache not found; skipped apt update candidate check"
    return
  fi
  if ! command -v dpkg >/dev/null 2>&1; then
    echo "WARN: dpkg not found; skipped apt update candidate check"
    return
  fi

  local native_arch
  native_arch="$(dpkg --print-architecture)"
  local apt_root="${TMPDIR}/apt"
  local sources="${TMPDIR}/beam.list"
  local keyring="${TMPDIR}/beam-apt.gpg"

  gpg --batch --yes --dearmor -o "${keyring}" "${TMPDIR}/beam.gpg"

  mkdir -p \
    "${apt_root}/cache" \
    "${apt_root}/lists/partial" \
    "${apt_root}/archives/partial" \
    "${apt_root}/sourceparts"
  : > "${apt_root}/status"
  printf 'deb [arch=%s signed-by=%s] %s stable main\n' "${native_arch}" "${keyring}" "${REPO_URL}" > "${sources}"

  local apt_opts=(
    -o "Dir::Etc::sourcelist=${sources}"
    -o "Dir::Etc::sourceparts=${apt_root}/sourceparts"
    -o "Dir::State::status=${apt_root}/status"
    -o "Dir::State::lists=${apt_root}/lists"
    -o "Dir::Cache=${apt_root}/cache"
    -o "Dir::Cache::archives=${apt_root}/archives"
    -o "Debug::NoLocking=true"
  )

  apt-get "${apt_opts[@]}" update -qq

  local candidate
  candidate="$(apt-cache "${apt_opts[@]}" policy beam | awk '/Candidate:/ && candidate == "" { candidate = $2 } END { print candidate }')"
  if [[ -z "${candidate}" || "${candidate}" == "(none)" ]]; then
    echo "ERROR: apt update completed, but no beam candidate was visible" >&2
    exit 1
  fi
  if [[ -n "${EXPECTED_VERSION}" && "${candidate}" != "${EXPECTED_VERSION}" ]]; then
    echo "ERROR: apt candidate mismatch" >&2
    echo "  expected: ${EXPECTED_VERSION}" >&2
    echo "  actual:   ${candidate}" >&2
    exit 1
  fi

  echo "OK: apt update exposes beam ${candidate} for ${native_arch}"
}

require_command curl
require_command gpg
require_command gpgv
require_command sha256sum
require_command awk

fetch "gpg/beam.gpg" "${TMPDIR}/beam.gpg"
fetch "dists/stable/InRelease" "${TMPDIR}/InRelease"
fetch "dists/stable/Release" "${TMPDIR}/Release"
fetch "dists/stable/Release.gpg" "${TMPDIR}/Release.gpg"

gpg --batch --no-default-keyring --keyring "${TMPDIR}/beam-keyring.gpg" --import "${TMPDIR}/beam.gpg" >/dev/null 2>&1
gpgv --keyring "${TMPDIR}/beam-keyring.gpg" "${TMPDIR}/InRelease" >/dev/null 2>&1
gpgv --keyring "${TMPDIR}/beam-keyring.gpg" "${TMPDIR}/Release.gpg" "${TMPDIR}/Release" >/dev/null 2>&1

fingerprint="$(gpg --show-keys --with-colons "${TMPDIR}/beam.gpg" | awk -F: '$1 == "fpr" && fingerprint == "" { fingerprint = $10 } END { print fingerprint }')"
echo "OK: APT signatures verify with key ${fingerprint}"

for arch in amd64 arm64; do
  packages_path="dists/stable/main/binary-${arch}/Packages"
  packages_file="${TMPDIR}/Packages-${arch}"
  fetch "${packages_path}" "${packages_file}"
  verify_sha256 "${packages_path}" "${packages_file}" "$(release_sha256_for "main/binary-${arch}/Packages")"

  version="$(package_field Version "${packages_file}")"
  filename="$(package_field Filename "${packages_file}")"
  deb_sha256="$(package_field SHA256 "${packages_file}")"

  if [[ -z "${version}" || -z "${filename}" || -z "${deb_sha256}" ]]; then
    echo "ERROR: incomplete beam package metadata for ${arch}" >&2
    exit 1
  fi
  if [[ -z "${EXPECTED_VERSION}" ]]; then
    EXPECTED_VERSION="${version}"
  fi
  if [[ "${version}" != "${EXPECTED_VERSION}" ]]; then
    echo "ERROR: ${arch} package version mismatch" >&2
    echo "  expected: ${EXPECTED_VERSION}" >&2
    echo "  actual:   ${version}" >&2
    exit 1
  fi

  deb_file="${TMPDIR}/beam-${arch}.deb"
  fetch "${filename}" "${deb_file}"
  verify_sha256 "${filename}" "${deb_file}" "${deb_sha256}"
  echo "OK: ${arch} metadata and .deb checksum match for beam ${version}"
done

verify_apt_candidate
