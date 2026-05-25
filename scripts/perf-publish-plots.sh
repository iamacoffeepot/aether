#!/usr/bin/env bash
# iamacoffeepot/aether#1155: publish the candidate span-distribution PNGs
# (rendered by `aether-perf-plot` into $PLOT_DIR) to the `perf-plots`
# branch — a blob store with no source history — under pr-<N>/, then
# append inline image embeds to the sticky-comment body ($MD_OUT).
#
# Why a branch: GitHub's comment image proxy (camo) fetches `![](url)`
# anonymously, so the PNGs need a public URL it can reach. The repo is
# public, so a raw URL to a committed file renders inline; an artifact
# URL (auth-gated) would not. Fork PRs get a read-only token, so the push
# fails — the caller runs this with `continue-on-error` and the artifact
# upload stays as the fallback.
#
# Env: GH_TOKEN, REPO (owner/name), PR (number), PLOT_DIR, MD_OUT.
set -euo pipefail

branch=perf-plots
dir="pr-${PR}"

shopt -s nullglob
pngs=("$PLOT_DIR"/*.png)
if [ ${#pngs[@]} -eq 0 ]; then
    echo "perf-publish-plots: no PNGs in $PLOT_DIR; nothing to publish" >&2
    exit 0
fi

# Work in a throwaway clone so the build checkout is untouched. If the
# branch exists, extend it (preserving other PRs' dirs); else start it
# orphan (no source history).
pp="$(mktemp -d)"
url="https://x-access-token:${GH_TOKEN}@github.com/${REPO}.git"
if git clone --quiet --depth 1 --branch "$branch" "$url" "$pp" 2>/dev/null; then
    rm -rf "${pp:?}/${dir}"
else
    git clone --quiet --depth 1 "$url" "$pp"
    git -C "$pp" checkout --quiet --orphan "$branch"
    git -C "$pp" rm -rfq . 2>/dev/null || true
fi

mkdir -p "$pp/$dir"
cp "${pngs[@]}" "$pp/$dir/"
git -C "$pp" config user.name "github-actions[bot]"
git -C "$pp" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git -C "$pp" add "$dir"
git -C "$pp" commit --quiet -m "perf plots: PR #${PR}"
git -C "$pp" push --quiet origin "HEAD:${branch}"

# Append inline embeds to the sticky-comment body.
{
    printf '\n<details><summary>span distributions (candidate)</summary>\n\n'
    for f in "${pngs[@]}"; do
        n="$(basename "$f" .png)"
        printf '![%s](https://github.com/%s/raw/%s/%s/%s.png)\n\n' \
            "$n" "$REPO" "$branch" "$dir" "$n"
    done
    printf '</details>\n'
} >> "$MD_OUT"

echo "perf-publish-plots: published ${#pngs[@]} plot(s) to ${branch}/${dir}, embedded in ${MD_OUT}" >&2
