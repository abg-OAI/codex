#!/usr/bin/env bash

set -euo pipefail

usage() {
	cat <<'EOF'
Usage: tools/saffrodex-tag-and-push.sh [-b BODY] [--dry-run] [--remote REMOTE]

Create and push the next Saffrodex release tag for the current Frodex base.

Options:
  -b BODY          Create an annotated tag with BODY after the release subject.
  --dry-run        Print the next version without tagging or pushing.
  --remote REMOTE  Push to REMOTE (default: origin).
  -h, --help       Show this help.
EOF
}

die() {
	echo "release.sh: $*" >&2
	exit 1
}

dry_run=false
remote=origin
body=

while (($# > 0)); do
	case "$1" in
	-b)
		(($# >= 2)) || die "-b requires a value"
		[[ -n "$2" ]] || die "-b requires a non-empty body"
		body=$2
		shift 2
		;;
	--dry-run)
		dry_run=true
		shift
		;;
	--remote)
		(($# >= 2)) || die "--remote requires a value"
		remote=$2
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) ||
	die "run this command from the Saffrodex repository"
cd "$repo_root"

branch=$(git symbolic-ref --quiet --short HEAD) ||
	die "cannot release from a detached HEAD"
[[ "$branch" == saffrodex ]] ||
	die "releases must be cut from the saffrodex branch, not $branch"

[[ -z $(git status --porcelain) ]] ||
	die "working tree and index must be clean"

frodex_tag=$(git describe --tags --match 'frodex-v*' --abbrev=0 HEAD 2>/dev/null) ||
	die "HEAD has no Frodex release tag in its ancestry"
frodex_version=${frodex_tag#frodex-v}
if [[ ! "$frodex_version" =~ ^.+\+frodex\.[0-9]+$ ]]; then
	die "invalid Frodex release tag: $frodex_tag"
fi

existing_release=$(git tag --points-at HEAD --list 'saffrodex-v*')
[[ -z "$existing_release" ]] ||
	die "HEAD is already released as ${existing_release//$'\n'/, }"

tag_prefix="saffrodex-v${frodex_version}.saffrodex."
tag_list=$(mktemp "${TMPDIR:-/tmp}/saffrodex-release.XXXXXX")
trap 'rm -f -- "$tag_list"' EXIT

git tag --list "${tag_prefix}*" >"$tag_list"
git ls-remote --refs --tags "$remote" "refs/tags/${tag_prefix}*" |
	sed 's#^[^[:space:]]*[[:space:]]*refs/tags/##' >>"$tag_list"

release_number=0
while IFS= read -r candidate; do
	[[ -n "$candidate" ]] || continue
	suffix=${candidate#"$tag_prefix"}
	[[ "$suffix" =~ ^[0-9]+$ ]] ||
		die "invalid Saffrodex release tag: $candidate"
	if ((suffix >= release_number)); then
		release_number=$((suffix + 1))
	fi
done <"$tag_list"

version="${frodex_version}.saffrodex.${release_number}"
tag="saffrodex-v${version}"

if "$dry_run"; then
	printf '%s\n' "$version"
	exit 0
fi

if [[ -n "$body" ]]; then
	git tag --annotate "$tag" \
		--message "saffrodex $version" \
		--message "$body"
else
	git tag "$tag"
fi

# The atomic push either publishes the rolling branch and release tag together
# or leaves the remote unchanged. Remove only this invocation's local tag when
# the push fails so a retry computes the same release number.
if ! git push --atomic "$remote" \
	"HEAD:refs/heads/$branch" \
	"refs/tags/$tag:refs/tags/$tag"; then
	git tag --delete "$tag" >/dev/null
	die "push failed; removed local tag $tag"
fi

printf '%s\n' "$version"
