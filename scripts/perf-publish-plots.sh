#!/usr/bin/env bash
# iamacoffeepot/aether#1155: publish the candidate span-distribution PNGs
# (rendered by `aether-perf-plot` into $PLOT_DIR) to the `perf-plots`
# branch — a blob store with no source history — under pr-<N>/, then embed
# inline image references into the sticky-comment body ($MD_OUT).
#
# iamacoffeepot/aether#1228: instead of one end-of-comment lump, each plot
# is co-located under the report section it describes. `perf-plot` prefixes
# every PNG with its tier section name (`{tier}__{topo}-{workers}w.png`,
# tier ∈ latency / latency.heavy / latency.real) and `report.rs` emits a
# per-section anchor `<!-- aether-perf-plots: TIER -->`. This script
# find-replaces each anchor with that tier's plots wrapped in one collapsed
# <details>. The throughput section gets no anchor (perf-plot is latency-only).
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

# Co-locate each section's plots at its anchor (iamacoffeepot/aether#1228).
# For each `<!-- aether-perf-plots: TIER -->` anchor in $MD_OUT, replace it
# with a collapsed <details> holding the plots whose filename begins with
# `TIER__`. Anchors with no matching plots collapse to nothing; plots whose
# tier matched no anchor (shouldn't happen) are appended at the end so none
# are silently dropped. The single `<!-- aether-perf-report -->` sticky
# marker is untouched — the upsert finds the comment by it.
#
# Render one section's <details> block for tier $1 to stdout (empty if the
# tier has no plots). The URL embeds the full basename (the committed file
# name, prefix included).
emit_section_plots() {
    local tier="$1"
    local prefix="${tier}__"
    local any=0
    for f in "${pngs[@]}"; do
        local n
        n="$(basename "$f" .png)"
        case "$n" in
            "$prefix"*) ;;
            *) continue ;;
        esac
        if [ "$any" -eq 0 ]; then
            printf '<details><summary>span distributions (candidate)</summary>\n\n'
            any=1
        fi
        printf '![%s](https://github.com/%s/raw/%s/%s/%s.png)\n\n' \
            "$n" "$REPO" "$branch" "$dir" "$n"
    done
    if [ "$any" -eq 1 ]; then
        printf '</details>\n\n'
    fi
}

# Tiers a plot filename can carry, longest-prefix-first so `latency.heavy`
# is tested before the bare `latency` (otherwise `latency.heavy__...` would
# match the `latency__` prefix test — it doesn't, the `__` is exact, but the
# explicit order documents the intent and guards a future bare-`latency`
# glob). Mirrors report.rs's anchored section names.
tiers=(latency.heavy latency.real latency)

out="$(mktemp)"
matched_tiers=""
while IFS= read -r line || [ -n "$line" ]; do
    anchored=""
    for tier in "${tiers[@]}"; do
        if [ "$line" = "<!-- aether-perf-plots: ${tier} -->" ]; then
            anchored="$tier"
            break
        fi
    done
    if [ -n "$anchored" ]; then
        emit_section_plots "$anchored"
        matched_tiers="${matched_tiers} ${anchored}"
    else
        printf '%s\n' "$line"
    fi
done <"$MD_OUT" >"$out"

# Fallback: any plot whose tier never matched an anchor is appended so it
# isn't silently lost (e.g. a tier with plots but no rendered section).
{
    for tier in "${tiers[@]}"; do
        case " ${matched_tiers} " in
            *" ${tier} "*) continue ;;
        esac
        emit_section_plots "$tier"
    done
} >>"$out"

mv "$out" "$MD_OUT"

echo "perf-publish-plots: published ${#pngs[@]} plot(s) to ${branch}/${dir}, co-located in ${MD_OUT}" >&2
