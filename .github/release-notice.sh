#!/usr/bin/env bash
# Tells each issue a release fixes that the fix is out. The issues are the "(#N)" a changeset ends
# with, as they appear in the release notes; pull requests are skipped.
#
# Each issue gets one comment for the first RC that has its fix and one for the stable release; a
# hidden marker in the comment keeps later RCs and reruns from repeating it. The stable comment also
# drops the "fixed in next release" label. Issues stay open: closing is left to a person, once the
# reporter has had a chance to say whether it works.
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
if [ "$(gh release view "$TAG" --json isPrerelease --jq .isPrerelease)" = true ]; then
  marker="<!-- release-notice:preview -->"
  text="The fix is in the preview [Arke $version]($url). To try it now, set Settings › Updates › Channel to RC; the stable release will follow."
else
  marker="<!-- release-notice:stable -->"
  text="The fix is out in [Arke $version]($url). Arke updates itself, or you can get it from the release page. If it still happens for you, please say so here."
fi

numbers="$( (grep -oE '\(#[0-9]+\)' <<<"$body" || true) | tr -dc '0-9\n' | sort -nu)"
if [ -z "$numbers" ]; then
  echo "no issues in the notes of $TAG"
  exit 0
fi

for n in $numbers; do
  read -r is_pr state < <(gh api "repos/$GH_REPO/issues/$n" --jq '"\(has("pull_request")) \(.state)"')
  if [ "$is_pr" = true ]; then
    echo "#$n: pull request, skipped"
    continue
  fi
  if gh api --paginate "repos/$GH_REPO/issues/$n/comments" --jq '.[].body' | grep -qF "$marker"; then
    echo "#$n: already told, skipped"
    continue
  fi
  if $dry_run; then
    echo "#$n ($state): would comment: $text"
    continue
  fi
  gh issue comment "$n" --body "$text"$'\n\n'"$marker"
  if [[ "$marker" == *stable* ]]; then
    gh issue edit "$n" --remove-label "fixed in next release" >/dev/null 2>&1 || true
  fi
  echo "#$n: commented"
done
