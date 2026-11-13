#!/usr/bin/env bash
# Checks that the pushed tag is the version xtask publishes: the Cargo.toml version for stable
# (after cargo xtask version), the next version plus -rc.N for an RC. Writes channel= and number=
# to $GITHUB_OUTPUT. Used by every job of the release workflow that needs them.
set -euo pipefail
tag="${GITHUB_REF_NAME#v}"
case "$tag" in
  *-rc.*) channel=rc; number="${tag##*-rc.}" ;;
  *-*) echo "::error::unrecognized tag $GITHUB_REF_NAME (expected v1.2.3 or v1.2.3-rc.N)"; exit 1 ;;
  *) channel=stable; number="" ;;
esac
args=("$channel")
if [ -n "$number" ]; then
  if ! [[ "$number" =~ ^[0-9]+$ ]]; then
    echo "::error::RC number in $GITHUB_REF_NAME is not a number"
    exit 1
  fi
  args+=(--number "$number")
fi
expected="$(cargo xtask release-version "${args[@]}")"
if [ "$expected" != "$tag" ]; then
  echo "::error::tag $GITHUB_REF_NAME doesn't match the version to publish, v$expected"
  exit 1
fi
echo "channel=$channel" >> "$GITHUB_OUTPUT"
echo "number=$number" >> "$GITHUB_OUTPUT"
