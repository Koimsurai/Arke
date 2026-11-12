#!/bin/sh
# Checks a commit message against Conventional Commits.
# Usage: check-commit-msg.sh <file>   (git commit-msg hook)
#        check-commit-msg.sh -s "<subject>"   (CI, one subject per call)

if [ "$1" = "-s" ]; then
  subject=$2
else
  subject=$(grep -v '^#' "$1" | head -n 1)
fi

case $subject in
  Merge\ *|Revert\ *|fixup!\ *|squash!\ *) exit 0 ;;
esac

types='feat|fix|refactor|perf|test|docs|build|ci|chore|style|revert'
if ! printf '%s\n' "$subject" | grep -Eq "^($types)(\([a-z0-9._/-]+\))?!?: [^ ].*[^.]$"; then
  echo "commit message must follow Conventional Commits, e.g. 'feat(search): add size filter'" >&2
  echo "  got: $subject" >&2
  exit 1
fi
if [ "${#subject}" -gt 72 ]; then
  echo "commit subject is ${#subject} characters; keep it within 72" >&2
  exit 1
fi
