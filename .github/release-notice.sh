#!/usr/bin/env bash
# Tells each issue a release fixes that the fix is out. The issues are the "(#N)" or "(#N, #M)" a
# changeset ends with, as they appear in the release notes; pull requests are skipped.
#
# Each issue gets one comment for the first RC of a version that has its fix and one for that
# version's stable release; a hidden marker plus the release link in the comment keeps later RCs and
# reruns from repeating it, while a later version that fixes the same issue again still says so
# (#129 was fixed in 0.6 and again in 0.7, and 0.7 skipped it). The stable release also
# closes the issue and drops the "fixed in next release" label; an issue reopened after that (the
# marker is there) is left alone. An issue labeled "needs testing" (a fix only the reporter's own
# setup can confirm) is asked to try it instead, and stays open.
#
#   TAG=v0.4.1 GH_REPO=owner/name bash .github/release-notice.sh [--dry-run]
set -euo pipefail
dry_run=false
[ "${1:-}" = "--dry-run" ] && dry_run=true
: "${TAG:?TAG is required}"
: "${GH_REPO:?GH_REPO is required}"

body="$(gh release view "$TAG" --json body --jq .body)"
url="$(gh release view "$TAG" --json url --jq .url)"
version="${TAG#v}"
# What a comment about this version links to: any RC of it for a preview, the release itself for
# stable.
if [ "$(gh release view "$TAG" --json isPrerelease --jq .isPrerelease)" = true ]; then
  seen="/releases/tag/v${version%%-*}-"
  marker="<!-- release-notice:preview -->"
  text="The fix is in the preview [Arke $version]($url). To try it now, set Settings › Updates › Channel to RC; the stable release will follow."
else
  seen="/releases/tag/$TAG)"
  marker="<!-- release-notice:stable -->"
  text="The fix is out in [Arke $version]($url). Arke updates itself, or you can get it from the release page. If it still happens for you, please say so here."
  ask="A fix is out in [Arke $version]($url). Arke updates itself, or you can get it from the release page. Could you try it and say here whether it works for you? This issue stays open until then."
fi

# "(#N)", or several fixed together: "(#253, #255)".
numbers="$( { grep -oE '\(#[0-9]+(, #[0-9]+)*\)' <<<"$body" | grep -oE '[0-9]+' || true; } | sort -nu)"
if [ -z "$numbers" ]; then
  echo "no issues in the notes of $TAG"
  exit 0
fi

for n in $numbers; do
  read -r is_pr state testing < <(gh api "repos/$GH_REPO/issues/$n" \
    --jq '"\(has("pull_request")) \(.state) \(any(.labels[]; .name == "needs testing"))"')
  if [ "$is_pr" = true ]; then
    echo "#$n: pull request, skipped"
    continue
  fi
  told="$(gh api --paginate "repos/$GH_REPO/issues/$n/comments" \
    --jq ".[] | select(.body | contains(\"$marker\") and contains(\"$seen\")) | .id")"
  if [ -n "$told" ]; then
    echo "#$n: already told, skipped"
    continue
  fi
  stable=false
  [[ "$marker" == *stable* ]] && stable=true
  say="$text"
  keep=false
  if $stable && [ "$testing" = true ]; then
    say="$ask"
    keep=true
  fi
  if $dry_run; then
    echo "#$n ($state): would comment: $say"
    if $stable && ! $keep && [ "$state" = open ]; then
      echo "#$n: would close"
    fi
    continue
  fi
  # An issue that was converted to a discussion (or locked) takes no comments: note it and go on, so
  # one such issue doesn't keep the rest from being told.
  if ! gh issue comment "$n" --body "$say"$'\n\n'"$marker"; then
    echo "::warning::#$n: can't be commented on (converted to a discussion or locked), skipped"
    continue
  fi
  if $keep; then
    echo "#$n: asked to try it, left open"
    continue
  fi
  if $stable; then
    gh issue edit "$n" --remove-label "fixed in next release" >/dev/null 2>&1 || true
    if [ "$state" = open ]; then
      gh issue close "$n" --reason completed
      echo "#$n: commented and closed"
      continue
    fi
  fi
  echo "#$n: commented"
done
