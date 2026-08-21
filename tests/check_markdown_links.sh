#!/usr/bin/env bash
# The docs-class gate's enforcement point (ENG-7).
#
# AGENTS.md's verification table has said "docs/markdown only ⇒ markdown
# link/anchor check only — no cargo gate" since 2026-07-18, and no such
# check existed: the cheap tier of the gate regime was the honor-system
# tier. This is it — relative link targets and in-document anchors across
# every tracked markdown file.
#
# What it checks (and deliberately not more):
#   * inline `[text](target)` links whose target is a repo-relative path:
#     the file (or directory) must exist;
#   * `[text](#anchor)` and `[text](path.md#anchor)`: the anchor must match
#     a heading in the target file, GitHub's slug rules (lowercased,
#     non-alphanumerics dropped, spaces -> `-`), or an explicit
#     `<a name="…">` / `id="…"` anchor;
#   * nothing about http(s) URLs — a link checker that reaches the network
#     turns an offline gate into a flake.
#
# Usage:
#   tests/check_markdown_links.sh              # every tracked *.md
#   tests/check_markdown_links.sh a.md b.md    # only these files
set -uo pipefail
cd "$(dirname "$0")/.."

if [[ $# -gt 0 ]]; then
  FILES=("$@")
else
  mapfile -t FILES < <(git ls-files '*.md' '*.MD')
fi

# Heading slugs + explicit anchors for one file, one per line.
anchors_of() {
  local f="$1"
  [[ -f "$f" ]] || return 0
  # ATX headings -> GitHub slugs. Fenced code blocks are skipped so that a
  # `# comment` inside a shell snippet is not mistaken for a heading.
  awk '
    /^```/ { fence = !fence; next }
    !fence && /^#{1,6}[ \t]/ {
      sub(/^#{1,6}[ \t]+/, "")
      sub(/[ \t]+#*[ \t]*$/, "")
      print
    }
  ' "$f" | while IFS= read -r h; do
    printf '%s\n' "$h" |
      tr '[:upper:]' '[:lower:]' |
      sed -e 's/`//g' -e 's/\*//g' -e 's/\[\([^]]*\)\](\([^)]*\))/\1/g' \
        -e 's/[^a-z0-9 _-]//g' -e 's/ /-/g'
  done
  # Explicit HTML anchors.
  grep -oE '<a[^>]+name="[^"]+"|id="[^"]+"' "$f" 2>/dev/null |
    sed -e 's/.*"\([^"]*\)"$/\1/' || true
}

declare -i checked=0 bad=0

for f in "${FILES[@]}"; do
  [[ -f "$f" ]] || continue
  dir="$(dirname "$f")"
  lineno=0
  while IFS= read -r line; do
    lineno=$((lineno + 1))
    # Every inline link target on the line.
    while IFS= read -r target; do
      [[ -n "$target" ]] || continue
      case "$target" in
        http://* | https://* | mailto:* | tel:*) continue ;;
      esac
      checked+=1
      path="${target%%#*}"
      anchor="${target#*#}"
      [[ "$target" == *#* ]] || anchor=""
      if [[ -z "$path" ]]; then
        tfile="$f"
      elif [[ "$path" == /* ]]; then
        # Absolute paths name a filesystem, not this repo — out of scope.
        continue
      else
        tfile="$dir/$path"
      fi
      if [[ ! -e "$tfile" ]]; then
        echo "$f:$lineno: missing link target '$target'" >&2
        bad+=1
        continue
      fi
      if [[ -n "$anchor" && "$tfile" == *.md && -f "$tfile" ]]; then
        want="$(printf '%s' "$anchor" | tr '[:upper:]' '[:lower:]')"
        if ! anchors_of "$tfile" | grep -qxF "$want"; then
          echo "$f:$lineno: missing anchor '#$anchor' in '$tfile'" >&2
          bad+=1
        fi
      fi
    done < <(printf '%s\n' "$line" | grep -oE '\]\([^)[:space:]]+\)' | sed -e 's/^](//' -e 's/)$//')
  done <"$f"
done

echo "markdown link check: ${#FILES[@]} files, $checked local links, $bad broken"
if ((bad > 0)); then
  echo "markdown link check: FAIL" >&2
  exit 1
fi
echo "markdown link check: PASS"
