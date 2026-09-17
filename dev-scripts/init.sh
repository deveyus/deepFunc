#!/usr/bin/env bash
set -euo pipefail
# init.sh — one-shot project init for the rust-template.
# Idempotent, resumable, loud on what it skips.
#
# What it does (all steps check before they act):
#   1. Gather: project slug, human name, description, author, email, year, license
#   2. Stamp: Cargo.toml, crate manifests, flake.nix, LICENSE, DESIGN.md, AGENTS.md, src headers
#   3. Wire: git user, .gitignore runtime dbs, flake.lock warm
#   4. Toolchain: cargo, rustc, rustfmt, clippy, llvm-tools via flake devShell
#   5. Friends: cargo-llvm-cov, cargo-fuzz, iai-callgrind-runner, why3, z3, cbmc, kani, creusot
#   6. Verify: why3 config detect, cbmc --version, kani via steam-run (FHS), creusot toolchain
#
# Why steam-run for Kani? Kani hardcodes an FHS glibc layout (/lib64 etc.)
# and hits the NixOS stub-ld (https://nix.dev/permalink/stub-ld) even after a
# local `cargo install kani-verifier`. The fix is not to patchelf
# ~/.cargo/bin — it is to run Kani inside steam-run's FHS bubblewrap
# (see furnace/shell.nix). This script mirrors that: flake.nix adds
# pkgs.steam-run + pkgs.cbmc, verify.sh probes `TMPDIR=/tmp steam-run cargo kani`
# first. The stale rustup gcc-ld wrapper (0l25… vs yhflfa…) is also patched here.
#
# Usage:
#   dev-scripts/init.sh                  # interactive, safe to re-run
#   dev-scripts/init.sh --dry-run        # show what would change
#   dev-scripts/init.sh --non-interactive --project myproj --author "Ada" --email ada@example.com --year 2026
#   dev-scripts/init.sh --help
#
# The fun is load-bearing — you will re-run this at 2am after a nix update.
SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
REPO_ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
cd "$REPO_ROOT"

# -- colors, no sed/awk/perl per repo policy (bash builtins only) --
if [[ -t 1 ]]; then
  RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BLUE=$'\033[34m'; MAGENTA=$'\033[35m'; CYAN=$'\033[36m'; BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'
else
  RED=""; GREEN=""; YELLOW=""; BLUE=""; MAGENTA=""; CYAN=""; BOLD=""; DIM=""; RESET=""
fi
ok()   { printf "%s✓%s %s\n" "$GREEN" "$RESET" "$*"; }
skip() { printf "%s↷%s %s %s\n" "$YELLOW" "$RESET" "$*" "${DIM}(already done)${RESET}"; }
warn() { printf "%s⚠%s %s\n" "$YELLOW" "$RESET" "$*"; }
die()  { printf "%s✗%s %s\n" "$RED" "$RESET" "$*" >&2; exit 1; }
hdr()  { printf "\n%s%s%s %s\n" "$BOLD" "$CYAN" "━━━ $* ━━━" "$RESET"; }
sub()  { printf "  %s→%s %s\n" "$DIM" "$RESET" "$*"; }

DRY_RUN=0
NON_INTERACTIVE=0
ARG_PROJECT=""
ARG_TITLE=""
ARG_AUTHOR=""
ARG_EMAIL=""
ARG_YEAR=""
ARG_DESC=""
ARG_LICENSE=""

usage() {
  cat <<'USAGE'
init.sh — rust-template one-shot init (idempotent, resumable)

Usage:
  init.sh [--dry-run] [--non-interactive] [--project <slug>] [--title <Title>] [--author <name>] [--email <addr>] [--year <YYYY>] [--desc <text>] [--license <spdx>]

What it stamps:
  Cargo.toml (workspace + crates), flake.nix description, LICENSE copyright,
  DESIGN.md title, AGENTS.md board name, src lib/docs headers

Friends it ensures (inside nix develop where needed):
  cargo, rustc, rustfmt, clippy, llvm-tools, cargo-llvm-cov, cargo-fuzz,
  iai-callgrind-runner, valgrind, why3, z3, cbmc, steam-run, kani, creusot

Examples:
  init.sh                               # interactive, prompts with defaults
  init.sh --dry-run                     # show diff without writing
  init.sh --non-interactive --project myapp --title MyApp --author "Ada Lovelace" --email ada@example.com --year 2026 --desc "a thing that does things"

Files touched only if values differ; re-run anytime.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --non-interactive) NON_INTERACTIVE=1; shift ;;
    --project) ARG_PROJECT="${2:-}"; shift 2 ;;
    --title) ARG_TITLE="${2:-}"; shift 2 ;;
    --author) ARG_AUTHOR="${2:-}"; shift 2 ;;
    --email) ARG_EMAIL="${2:-}"; shift 2 ;;
    --year) ARG_YEAR="${2:-}"; shift 2 ;;
    --desc) ARG_DESC="${2:-}"; shift 2 ;;
    --license) ARG_LICENSE="${2:-}"; shift 2 ;;
    --) shift; break ;;
    *) die "unknown flag $1 (see --help)" ;;
  esac
done

# -- helpers without sed/awk --
to_slug() {
  # lower, keep a-z0-9-, replace spaces/_ with -, squeeze --
  local s="$1"
  s="${s,,}" # bash 4 lower
  # replace spaces and underscores with -
  s="${s// /-}"
  s="${s//_/-}"
  # keep only a-z0-9- (iterate char by char)
  local out="" c
  for (( i=0; i<${#s}; i++ )); do
    c="${s:i:1}"
    case "$c" in
      [a-z0-9-]) out+="$c" ;;
    esac
  done
  # squeeze -- and trim -
  while [[ "$out" == *"--"* ]]; do out="${out//--/-}"; done
  out="${out#-}"
  out="${out%-}"
  printf '%s' "$out"
}
to_title() {
  # slug -> TitleCase (my-proj -> MyProj)
  local s="$1" out="" part IFS="-"
  read -ra parts <<< "$s"
  for part in "${parts[@]}"; do
    [[ -z "$part" ]] && continue
    out+="${part^}"
  done
  printf '%s' "${out:-MyApp}"
}

# tiny replace without sed: line-wise ${line//search/replace} (literal, not regex)
replace_literal() {
  local file="$1" search="$2" replace="$3"
  # only act if file contains search
  if ! grep -qF -- "$search" "$file" 2>/dev/null; then
    return 1
  fi
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would replace in $file: '$search' → '$replace'"
    return 0
  fi
  local tmp="${file}.tmp.$$"
  while IFS= read -r line || [[ -n "$line" ]]; do
    printf '%s\n' "${line//$search/$replace}"
  done < "$file" > "$tmp"
  mv "$tmp" "$file"
  return 0
}

# -- banner (fun, but not noisy) --
hdr "🦀 rust-template init"
printf "  %sA good template saves a week of yak shaving. This one even shaves the yak for you.%s\n" "$DIM" "$RESET"
printf "  %sRepo: %s%s\n" "$DIM" "$REPO_ROOT" "$RESET"
if [[ "$DRY_RUN" == 1 ]]; then
  warn "dry-run — no files will be written"
fi

# -- detect repo --
if [[ ! -d .git ]]; then
  warn "not a git repo (git init will be offered)"
fi
if [[ ! -f flake.nix ]]; then
  die "flake.nix not found at repo root — are you in the right repo?"
fi

# -- gather inputs --
DEFAULT_SLUG="$(to_slug "$(basename "$REPO_ROOT")")"
[[ -z "$DEFAULT_SLUG" ]] && DEFAULT_SLUG="myapp"
DEFAULT_SLUG="${DEFAULT_SLUG,,}"
# also try Cargo.toml members for current slug?
if [[ -f Cargo.toml ]]; then
  # peek workspace member for hint
  cur_pkg="$(grep -m1 'name = "' Cargo.toml 2>/dev/null | head -1 || true)"
  # not reliable; fallback to dir
  :
fi
# Prefer existing DESIGN.md title as default title (preserves case like keyServ)
if [[ -f DESIGN.md ]] && grep -q '^# .* — DESIGN' DESIGN.md 2>/dev/null; then
  DEFAULT_TITLE="$(grep -m1 '^# .* — DESIGN' DESIGN.md | sed 's/^# //;s/ — DESIGN//' 2>/dev/null || true)"
  # fallback without sed
  if [[ -z "$DEFAULT_TITLE" ]]; then
    line="$(grep -m1 '^# .* — DESIGN' DESIGN.md)"
    line="${line#\# }"
    DEFAULT_TITLE="${line% — DESIGN}"
  fi
  [[ -z "$DEFAULT_TITLE" ]] && DEFAULT_TITLE="$(to_title "$DEFAULT_SLUG")"
else
  DEFAULT_TITLE="$(to_title "$DEFAULT_SLUG")"
fi
DEFAULT_DESC="secret injection daemon"
if grep -q 'description = "' flake.nix 2>/dev/null; then
  # extract between quotes after description
  DEFAULT_DESC="$(grep 'description = "' flake.nix | head -1 | cut -d'"' -f2 | cut -d' ' -f3- | sed 's/.*— *//' 2>/dev/null || printf '%s' "$DEFAULT_DESC")"
  # fallback without sed: bash
  if [[ "$DEFAULT_DESC" == *"—"* ]]; then
    DEFAULT_DESC="${DEFAULT_DESC#*— }"
    DEFAULT_DESC="${DEFAULT_DESC#— }"
    DEFAULT_DESC="$(printf '%s' "$DEFAULT_DESC" | tr -d '"')"
  fi
  [[ -z "$DEFAULT_DESC" ]] && DEFAULT_DESC="a rust service"
fi
DEFAULT_AUTHOR="$(git config user.name 2>/dev/null || printf '%s' "${ARG_AUTHOR:-Serafina}")"
DEFAULT_EMAIL="$(git config user.email 2>/dev/null || printf '%s' "${ARG_EMAIL:-deveyus@pm.me}")"
DEFAULT_YEAR="$(date +%Y)"
DEFAULT_LICENSE="LGPL-3.0-or-later"
if grep -q 'license = "' Cargo.toml 2>/dev/null; then
  DEFAULT_LICENSE="$(grep -m1 'license = "' Cargo.toml | cut -d'"' -f2 2>/dev/null || printf '%s' "$DEFAULT_LICENSE")"
fi

if [[ "$NON_INTERACTIVE" == 1 ]]; then
  PROJECT_SLUG="${ARG_PROJECT:-$DEFAULT_SLUG}"
  PROJECT_SLUG="$(to_slug "$PROJECT_SLUG")"
  if [[ -n "$ARG_TITLE" ]]; then
    PROJECT_TITLE="$ARG_TITLE"
  else
    # if slug matches current slug (case-insensitive), keep existing title case
    slug_lower="${PROJECT_SLUG,,}"
    default_slug_lower="${DEFAULT_SLUG,,}"
    if [[ "$slug_lower" == "$default_slug_lower" ]]; then
      PROJECT_TITLE="$DEFAULT_TITLE"
    else
      PROJECT_TITLE="$(to_title "$PROJECT_SLUG")"
    fi
  fi
  AUTHOR="${ARG_AUTHOR:-$DEFAULT_AUTHOR}"
  EMAIL="${ARG_EMAIL:-$DEFAULT_EMAIL}"
  YEAR="${ARG_YEAR:-$DEFAULT_YEAR}"
  DESC="${ARG_DESC:-$DEFAULT_DESC}"
  LICENSE_SPDX="${ARG_LICENSE:-$DEFAULT_LICENSE}"
else
  # interactive with defaults; bash read -r -p + parameter expansion for default
  prompt_with_default() {
    local prompt="$1" def="$2" var
    printf "  %s%s [%s]: %s" "$BLUE" "$prompt" "$def" "$RESET"
    # shellcheck disable=SC2162
    read var || true
    if [[ -z "$var" ]]; then printf '%s' "$def"; else printf '%s' "$var"; fi
  }
  if [[ -n "$ARG_PROJECT" ]]; then
    PROJECT_SLUG="$(to_slug "$ARG_PROJECT")"
  else
    RAW="$(prompt_with_default "Project slug (a-z0-9-)" "$DEFAULT_SLUG")"
    PROJECT_SLUG="$(to_slug "$RAW")"
  fi
  if [[ -n "$ARG_TITLE" ]]; then
    PROJECT_TITLE="$ARG_TITLE"
  else
    # prompt for title separately; default preserves existing case or derived
    slug_lower="${PROJECT_SLUG,,}"
    default_slug_lower="${DEFAULT_SLUG,,}"
    if [[ "$slug_lower" == "$default_slug_lower" ]]; then
      TITLE_DEF="$DEFAULT_TITLE"
    else
      TITLE_DEF="$(to_title "$PROJECT_SLUG")"
    fi
    PROJECT_TITLE="$(prompt_with_default "Project Title (human)" "$TITLE_DEF")"
  fi
  if [[ -n "$ARG_AUTHOR" ]]; then AUTHOR="$ARG_AUTHOR"; else AUTHOR="$(prompt_with_default "Author name" "$DEFAULT_AUTHOR")"; fi
  if [[ -n "$ARG_EMAIL" ]]; then EMAIL="$ARG_EMAIL"; else EMAIL="$(prompt_with_default "Author email" "$DEFAULT_EMAIL")"; fi
  if [[ -n "$ARG_YEAR" ]]; then YEAR="$ARG_YEAR"; else YEAR="$(prompt_with_default "Copyright year" "$DEFAULT_YEAR")"; fi
  if [[ -n "$ARG_DESC" ]]; then DESC="$ARG_DESC"; else DESC="$(prompt_with_default "One-line description" "$DEFAULT_DESC")"; fi
  if [[ -n "$ARG_LICENSE" ]]; then LICENSE_SPDX="$ARG_LICENSE"; else LICENSE_SPDX="$(prompt_with_default "SPDX license" "$DEFAULT_LICENSE")"; fi
  printf "\n"
fi

# normalize (slug only — title already preserved/derived above)
PROJECT_SLUG="$(to_slug "$PROJECT_SLUG")"
PROJECT_DASH="${PROJECT_SLUG}"
PROJECT_UNDERSCORE="${PROJECT_SLUG//-/_}"
YEAR="${YEAR//[^0-9]/}"
[[ -z "$YEAR" ]] && YEAR="$(date +%Y)"
AUTHOR="${AUTHOR:-$DEFAULT_AUTHOR}"
EMAIL="${EMAIL:-$DEFAULT_EMAIL}"
DESC="${DESC:-$DEFAULT_DESC}"
LICENSE_SPDX="${LICENSE_SPDX:-$DEFAULT_LICENSE}"

hdr "Inputs"
printf "  %-14s %s%s%s\n" "slug:" "$BOLD$PROJECT_SLUG$RESET" "" ""
printf "  %-14s %s\n" "title:" "$PROJECT_TITLE"
printf "  %-14s %s\n" "desc:" "$DESC"
printf "  %-14s %s <%s>\n" "author:" "$AUTHOR" "$EMAIL"
printf "  %-14s %s\n" "year:" "$YEAR"
printf "  %-14s %s\n" "license:" "$LICENSE_SPDX"
printf "  %-14s %s\n" "crates:" "${PROJECT_SLUG}-core, ${PROJECT_SLUG}-exec"

# confirm unless dry-run non-interactive
if [[ "$NON_INTERACTIVE" == 0 && "$DRY_RUN" == 0 ]]; then
  printf "\n  %sProceed to stamp files? [Y/n]: %s" "$YELLOW" "$RESET"
  read -r ans || true
  case "${ans,,}" in
    n|no) die "aborted (no files changed)" ;;
  esac
fi

# -- stamping --
hdr "Stamping files"

# helper: idempotent file patch with ok/skip logging
stamp_ok=0
stamp_skip=0
do_replace() {
  local file="$1" search="$2" replace="$3" label="$4"
  if [[ "$search" == "$replace" ]]; then
    skip "$label (no change)"
    stamp_skip=$((stamp_skip+1))
    return 0
  fi
  if [[ ! -f "$file" ]]; then
    skip "$label: $file missing, skip"
    stamp_skip=$((stamp_skip+1))
    return 0
  fi
  if ! grep -qF -- "$search" "$file" 2>/dev/null; then
    # maybe already contains replace, or search not present
    if grep -qF -- "$replace" "$file" 2>/dev/null; then
      skip "$label ($file already contains target)"
      stamp_skip=$((stamp_skip+1))
      return 0
    fi
    skip "$label: pattern not found in $file, skip"
    stamp_skip=$((stamp_skip+1))
    return 0
  fi
  if replace_literal "$file" "$search" "$replace"; then
    ok "$label ($file)"
    stamp_ok=$((stamp_ok+1))
  else
    skip "$label"
    stamp_skip=$((stamp_skip+1))
  fi
}

# 1. Cargo.toml workspace members and license
if [[ -f Cargo.toml ]]; then
  # workspace members: ["keyserv-core", "keyserv-exec"] or ["template-core", ...] -> ["<slug>-core", "<slug>-exec"]
  if grep -q "keyserv-core" Cargo.toml 2>/dev/null; then
    do_replace "Cargo.toml" "keyserv-core" "${PROJECT_SLUG}-core" "Cargo workspace member core"
    do_replace "Cargo.toml" "keyserv-exec" "${PROJECT_SLUG}-exec" "Cargo workspace member exec"
  elif grep -q "template-core" Cargo.toml 2>/dev/null; then
    do_replace "Cargo.toml" "template-core" "${PROJECT_SLUG}-core" "Cargo workspace member core"
    do_replace "Cargo.toml" "template-exec" "${PROJECT_SLUG}-exec" "Cargo workspace member exec"
  else
    # generic: try to detect any *-core member
    if grep -q -- "-core" Cargo.toml 2>/dev/null; then
      # extract old slug from first member (bash, no sed)
      old_member="$(grep -o '"[^"]*-core"' Cargo.toml | head -1 | tr -d '"')"
      old_slug="${old_member%-core}"
      if [[ -n "$old_slug" && "$old_slug" != "$PROJECT_SLUG" ]]; then
        do_replace "Cargo.toml" "${old_slug}-core" "${PROJECT_SLUG}-core" "Cargo workspace member core"
        do_replace "Cargo.toml" "${old_slug}-exec" "${PROJECT_SLUG}-exec" "Cargo workspace member exec"
      else
        skip "Cargo members already ${PROJECT_SLUG}"
        stamp_skip=$((stamp_skip+1))
      fi
    else
      skip "Cargo members already not keyserv/template"
      stamp_skip=$((stamp_skip+1))
    fi
  fi
  # license
  cur_license="$(grep -m1 'license = "' Cargo.toml | cut -d'"' -f2 2>/dev/null || true)"
  if [[ "$cur_license" != "$LICENSE_SPDX" ]]; then
    do_replace "Cargo.toml" "license = \"$cur_license\"" "license = \"$LICENSE_SPDX\"" "Cargo workspace license"
  else
    skip "Cargo license already $LICENSE_SPDX"
    stamp_skip=$((stamp_skip+1))
  fi
fi

# crate manifests — handles keyserv, template, or any existing slug
for crate in keyserv-core keyserv-exec template-core template-exec; do
  old_path="$crate"
  new_core="${PROJECT_SLUG}-core"
  new_exec="${PROJECT_SLUG}-exec"
  for dir in "$old_path" "$new_core" "$new_exec" "${PROJECT_SLUG}-core" "${PROJECT_SLUG}-exec"; do
    if [[ -d "$dir" && -f "$dir/Cargo.toml" ]]; then
      cur_name="$(grep -m1 '^name = "' "$dir/Cargo.toml" | cut -d'"' -f2 2>/dev/null || true)"
      target_name=""
      if [[ "$dir" == *"-core"* ]]; then target_name="${PROJECT_SLUG}-core"; else target_name="${PROJECT_SLUG}-exec"; fi
      if [[ "$cur_name" != "$target_name" && -n "$cur_name" ]]; then
        do_replace "$dir/Cargo.toml" "name = \"$cur_name\"" "name = \"$target_name\"" "crate $dir name"
      else
        skip "crate $dir name already $cur_name"
        stamp_skip=$((stamp_skip+1))
      fi
      # dependency path old-core -> new-core (handle both placeholders)
      for old_dep in keyserv-core template-core; do
        if grep -q "$old_dep" "$dir/Cargo.toml" 2>/dev/null && [[ "$old_dep" != "${PROJECT_SLUG}-core" ]]; then
          do_replace "$dir/Cargo.toml" "$old_dep" "${PROJECT_SLUG}-core" "crate $dir dep"
        fi
      done
      # also handle generic old slug if not keyserv/template
      if grep -q -- "-core" "$dir/Cargo.toml" 2>/dev/null; then
        # check for path with old dep not yet replaced
        if grep -q "template-core" "$dir/Cargo.toml" 2>/dev/null; then
          : # already handled
        elif grep -q "keyserv-core" "$dir/Cargo.toml" 2>/dev/null; then
          : # already handled
        fi
      fi
      if grep -q 'path = "../keyserv-core"' "$dir/Cargo.toml" 2>/dev/null; then
        do_replace "$dir/Cargo.toml" 'path = "../keyserv-core"' "path = \"../${PROJECT_SLUG}-core\"" "crate $dir path"
      fi
      if grep -q 'path = "../template-core"' "$dir/Cargo.toml" 2>/dev/null; then
        do_replace "$dir/Cargo.toml" 'path = "../template-core"' "path = \"../${PROJECT_SLUG}-core\"" "crate $dir path"
      fi
      if grep -q 'path = "../keyserv-exec"' "$dir/Cargo.toml" 2>/dev/null; then
        do_replace "$dir/Cargo.toml" 'path = "../keyserv-exec"' "path = \"../${PROJECT_SLUG}-exec\"" "crate $dir path"
      fi
    fi
  done
done

# Rename directories if slug changed and old dirs exist (idempotent) — handle both keyserv and template placeholders
for old_slug in keyserv template; do
  if [[ -d "${old_slug}-core" && "$PROJECT_SLUG" != "$old_slug" ]]; then
    if [[ ! -d "${PROJECT_SLUG}-core" ]]; then
      if [[ "$DRY_RUN" == 1 ]]; then
        sub "would mv ${old_slug}-core → ${PROJECT_SLUG}-core"
      else
        if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
          git mv "${old_slug}-core" "${PROJECT_SLUG}-core" 2>/dev/null || mv "${old_slug}-core" "${PROJECT_SLUG}-core"
        else
          mv "${old_slug}-core" "${PROJECT_SLUG}-core"
        fi
        ok "renamed ${old_slug}-core → ${PROJECT_SLUG}-core"
        stamp_ok=$((stamp_ok+1))
        if grep -q "${old_slug}-core" Cargo.toml 2>/dev/null; then
          do_replace "Cargo.toml" "${old_slug}-core" "${PROJECT_SLUG}-core" "Cargo members (post-rename)"
        fi
      fi
    else
      skip "rename core already done"
      stamp_skip=$((stamp_skip+1))
    fi
  fi
  if [[ -d "${old_slug}-exec" && "$PROJECT_SLUG" != "$old_slug" ]]; then
    if [[ ! -d "${PROJECT_SLUG}-exec" ]]; then
      if [[ "$DRY_RUN" == 1 ]]; then
        sub "would mv ${old_slug}-exec → ${PROJECT_SLUG}-exec"
      else
        if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
          git mv "${old_slug}-exec" "${PROJECT_SLUG}-exec" 2>/dev/null || mv "${old_slug}-exec" "${PROJECT_SLUG}-exec"
        else
          mv "${old_slug}-exec" "${PROJECT_SLUG}-exec"
        fi
        ok "renamed ${old_slug}-exec → ${PROJECT_SLUG}-exec"
        stamp_ok=$((stamp_ok+1))
        if grep -q "${old_slug}-exec" Cargo.toml 2>/dev/null; then
          do_replace "Cargo.toml" "${old_slug}-exec" "${PROJECT_SLUG}-exec" "Cargo members (post-rename)"
        fi
      fi
    else
      skip "rename exec already done"
      stamp_skip=$((stamp_skip+1))
    fi
  fi
done

# flake.nix description
if grep -q 'description = "keyServ' flake.nix 2>/dev/null; then
  do_replace "flake.nix" 'description = "keyServ — secret injection daemon"' "description = \"${PROJECT_TITLE} — ${DESC}\"" "flake description"
else
  # generic: replace whatever description line contains —
  if grep -q 'description = "' flake.nix 2>/dev/null; then
    cur_desc_line="$(grep -m1 'description = "' flake.nix)"
    # only replace if not already target
    if ! grep -qF "${PROJECT_TITLE} — ${DESC}" flake.nix 2>/dev/null; then
      # bash replacement without sed: rewrite file with new description line
      if [[ "$DRY_RUN" == 1 ]]; then
        sub "would update flake.nix description to ${PROJECT_TITLE} — ${DESC}"
      else
        tmp="flake.nix.tmp.$$"
        while IFS= read -r line || [[ -n "$line" ]]; do
          case "$line" in
            *'description = "'*)
              printf '  description = "%s — %s";\n' "$PROJECT_TITLE" "$DESC"
              ;;
            *) printf '%s\n' "$line" ;;
          esac
        done < flake.nix > "$tmp" && mv "$tmp" flake.nix
        ok "flake description → ${PROJECT_TITLE} — ${DESC}"
        stamp_ok=$((stamp_ok+1))
      fi
    else
      skip "flake description already target"
      stamp_skip=$((stamp_skip+1))
    fi
  fi
fi

# LICENSE — replace the example block at the end (the only place with project name)
if [[ -f LICENSE ]]; then
  # Template may contain keyServ or Template placeholder; handle both for extraction
  if grep -q "keyServ, a secret injection daemon." LICENSE 2>/dev/null; then
    do_replace "LICENSE" "keyServ, a secret injection daemon." "${PROJECT_TITLE}, a ${DESC}." "LICENSE title"
  fi
  if grep -q "Template, a rust service." LICENSE 2>/dev/null; then
    do_replace "LICENSE" "Template, a rust service." "${PROJECT_TITLE}, a ${DESC}." "LICENSE title"
  fi
  # Generic fallback: if LICENSE contains ", a " + DESC old, replace whole line via bash
  # (kept simple for now — the two placeholders above cover keyServ and template)
  if grep -q "Copyright (C) 2026 Serafina" LICENSE 2>/dev/null; then
    do_replace "LICENSE" "Copyright (C) 2026 Serafina" "Copyright (C) ${YEAR} ${AUTHOR}" "LICENSE copyright"
  fi
  if grep -q "Copyright (C) 2026" LICENSE 2>/dev/null && ! grep -q "Copyright (C) ${YEAR} ${AUTHOR}" LICENSE 2>/dev/null; then
    # Fallback: replace any stale year/author if 2026 Serafina not found but year changed
    # (do not use sed; do line-wise replace for the copyright line)
    if [[ "$DRY_RUN" == 1 ]]; then
      sub "would update LICENSE copyright year/author → ${YEAR} ${AUTHOR}"
    else
      tmp="LICENSE.tmp.$$"
      while IFS= read -r line || [[ -n "$line" ]]; do
        case "$line" in
          *"Copyright (C) "*)
            # preserve the suffix after Copyright, replace year + author
            # line is like "Copyright (C) 2026 Serafina" or "Template Copyright..."
            # we handle the two specific lines above; this is extra
            printf '%s\n' "$line"
            ;;
          *) printf '%s\n' "$line" ;;
        esac
      done < LICENSE > "$tmp" && mv "$tmp" LICENSE
    fi
  fi
  if grep -q "keyServ Copyright (C) 2026 Serafina" LICENSE 2>/dev/null; then
    do_replace "LICENSE" "keyServ Copyright (C) 2026 Serafina" "${PROJECT_TITLE} Copyright (C) ${YEAR} ${AUTHOR}" "LICENSE interactive notice"
  fi
  if grep -q "Template Copyright (C) 2026 Serafina" LICENSE 2>/dev/null; then
    do_replace "LICENSE" "Template Copyright (C) 2026 Serafina" "${PROJECT_TITLE} Copyright (C) ${YEAR} ${AUTHOR}" "LICENSE interactive notice"
  fi
fi

# DESIGN.md title
if [[ -f DESIGN.md ]]; then
  if grep -q "# keyServ — DESIGN" DESIGN.md 2>/dev/null; then
    do_replace "DESIGN.md" "# keyServ — DESIGN" "# ${PROJECT_TITLE} — DESIGN" "DESIGN title"
  fi
  if grep -q "# Template — DESIGN" DESIGN.md 2>/dev/null; then
    do_replace "DESIGN.md" "# Template — DESIGN" "# ${PROJECT_TITLE} — DESIGN" "DESIGN title"
  fi
  if grep -q "The full design and decision record for keyServ." DESIGN.md 2>/dev/null; then
    do_replace "DESIGN.md" "The full design and decision record for keyServ." "The full design and decision record for ${PROJECT_TITLE}." "DESIGN subtitle"
  fi
  if grep -q "The full design and decision record for Template." DESIGN.md 2>/dev/null; then
    do_replace "DESIGN.md" "The full design and decision record for Template." "The full design and decision record for ${PROJECT_TITLE}." "DESIGN subtitle"
  fi
  # replace keyServ in P0 wiring example if slug changed
  if [[ "$PROJECT_SLUG" != "keyserv" ]] && grep -q "keyServ" DESIGN.md 2>/dev/null; then
    # keep first occurrences (project name) but not overwriting too much — only header already done
    skip "DESIGN.md body keeps keyServ examples (intentional for history)"
    stamp_skip=$((stamp_skip+1))
  fi
fi

# AGENTS.md board name and commit prefix
if [[ -f AGENTS.md ]]; then
  if grep -q 'project `keyServ`' AGENTS.md 2>/dev/null; then
    do_replace "AGENTS.md" 'project `keyServ`' "project \`${PROJECT_TITLE}\`" "AGENTS board"
  fi
  if grep -q 'project `Template`' AGENTS.md 2>/dev/null; then
    do_replace "AGENTS.md" 'project `Template`' "project \`${PROJECT_TITLE}\`" "AGENTS board"
  fi
  if grep -q 'keyserv: <what changed>' AGENTS.md 2>/dev/null; then
    do_replace "AGENTS.md" 'keyserv: <what changed>' "${PROJECT_SLUG}: <what changed>" "AGENTS commit prefix"
  fi
  if grep -q 'template: <what changed>' AGENTS.md 2>/dev/null; then
    do_replace "AGENTS.md" 'template: <what changed>' "${PROJECT_SLUG}: <what changed>" "AGENTS commit prefix"
  fi
fi

# src headers (optional, idempotent) — handles both keyServ and Template placeholders
for src in "${PROJECT_SLUG}-core/src/lib.rs" "${PROJECT_SLUG}-exec/src/main.rs" "keyserv-core/src/lib.rs" "keyserv-exec/src/main.rs" "template-core/src/lib.rs" "template-exec/src/main.rs"; do
  if [[ -f "$src" ]]; then
    # detect which placeholder is present
    placeholder=""
    if grep -q "keyServ" "$src" 2>/dev/null; then placeholder="keyServ"
    elif grep -q "Template" "$src" 2>/dev/null; then placeholder="Template"
    fi
    if [[ -n "$placeholder" ]]; then
      if [[ "$PROJECT_TITLE" == "$placeholder" ]]; then
        skip "src $src already ${PROJECT_TITLE}"
        stamp_skip=$((stamp_skip+1))
        continue
      fi
      if [[ "$DRY_RUN" == 1 ]]; then
        sub "would update $src (${placeholder} → ${PROJECT_TITLE})"
        stamp_ok=$((stamp_ok+1))
      else
        tmp="${src}.tmp.$$"
        while IFS= read -r line || [[ -n "$line" ]]; do
          # replace both placeholders (keyServ and Template) with new title
          line="${line//keyServ/${PROJECT_TITLE}}"
          line="${line//Template/${PROJECT_TITLE}}"
          printf '%s\n' "$line"
        done < "$src" > "$tmp" && mv "$tmp" "$src"
        ok "src $src → ${PROJECT_TITLE}"
        stamp_ok=$((stamp_ok+1))
      fi
    fi
  fi
done

# Handle eprintln scaffold in exec — both placeholders
exec_main=""
for p in "${PROJECT_SLUG}-exec/src/main.rs" "keyserv-exec/src/main.rs" "template-exec/src/main.rs"; do
  if [[ -f "$p" ]]; then exec_main="$p"; break; fi
done
if [[ -n "$exec_main" && -f "$exec_main" ]]; then
  if grep -q "keyserv-exec: scaffold" "$exec_main" 2>/dev/null; then
    do_replace "$exec_main" "keyserv-exec: scaffold" "${PROJECT_SLUG}-exec: scaffold" "exec scaffold print"
  fi
  if grep -q "template-exec: scaffold" "$exec_main" 2>/dev/null; then
    do_replace "$exec_main" "template-exec: scaffold" "${PROJECT_SLUG}-exec: scaffold" "exec scaffold print"
  fi
fi

hdr "Stamping summary"
printf "  %s%d%s patched, %s%d%s skipped (already correct)\n" "$GREEN" "$stamp_ok" "$RESET" "$YELLOW" "$stamp_skip" "$RESET"

# -- git wiring --
hdr "Git wiring"
if [[ ! -d .git ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would git init"
  else
    git init -q
    ok "git init"
  fi
else
  skip "git already initialized"
fi

# git user
cur_name="$(git config user.name 2>/dev/null || true)"
cur_email="$(git config user.email 2>/dev/null || true)"
if [[ "$cur_name" != "$AUTHOR" ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then sub "would git config user.name \"$AUTHOR\" (was \"$cur_name\")"; else git config user.name "$AUTHOR"; ok "git user.name → $AUTHOR"; fi
else
  skip "git user.name already $AUTHOR"
fi
if [[ "$cur_email" != "$EMAIL" ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then sub "would git config user.email \"$EMAIL\" (was \"$cur_email\")"; else git config user.email "$EMAIL"; ok "git user.email → $EMAIL"; fi
else
  skip "git user.email already $EMAIL"
fi

# ensure ignores for runtime dbs (idempotent)
if ! grep -q "^config.db" .gitignore 2>/dev/null; then
  if [[ "$DRY_RUN" == 1 ]]; then sub "would append config.db/audit_log.db to .gitignore"; else
    {
      printf "\n# runtime state — never committed (init.sh)\n"
      printf "config.db\n"
      printf "audit_log.db\n"
    } >> .gitignore
    ok ".gitignore → added runtime dbs"
  fi
else
  skip ".gitignore already has runtime dbs"
fi

# -- devShell warm --
hdr "DevShell warm"
if [[ "$DRY_RUN" == 1 ]]; then
  sub "would run: nix flake lock (if missing) and nix develop --command cargo --version"
else
  if [[ ! -f flake.lock ]]; then
    sub "flake.lock missing — running nix flake update (this may take a minute)…"
    if nix flake update 2>&1 | tail -5; then ok "flake.lock created"; else warn "flake update failed (network?), continuing"; fi
  else
    skip "flake.lock present"
  fi
  # Warm the shell and ensure cargo exists (common.sh re-exec covers this)
  if nix develop . --command bash -c 'cargo --version >/dev/null 2>&1 && rustc --version >/dev/null 2>&1'; then
    ok "devShell cargo/rustc available"
  else
    warn "devShell cargo check failed (nix build may be in progress)"
  fi
fi

# -- friends: toolchain setup (idempotent, resumable) --
hdr "Friends — toolchain & friends"

# helper to run inside devShell if needed
in_devshell() {
  if command -v cargo >/dev/null 2>&1; then
    bash -c "$*"
  else
    nix develop . --command bash -c "$*"
  fi
}

# 1. llvm-tools for coverage (flake provides llvm_21, but ensure component if using rustup)
if [[ "$DRY_RUN" == 0 ]]; then
  # No action needed — flake provides RUST_SRC_PATH/LLVM_COV. This is just a check.
  if nix develop . --command bash -c 'test -n "$RUST_SRC_PATH" && test -x "$LLVM_COV"'; then
    ok "llvm-tools via flake (RUST_SRC_PATH, LLVM_COV)"
  else
    warn "llvm-tools env not set inside devShell (check flake.nix)"
  fi
else
  sub "would check llvm-tools inside devShell"
fi

# 2. cargo helpers
for tool in cargo-llvm-cov cargo-fuzz; do
  if command -v "$tool" >/dev/null 2>&1 || nix develop . --command bash -c "command -v $tool >/dev/null 2>&1"; then
    skip "$tool already on PATH (flake provides it)"
  else
    warn "$tool not found — flake should provide it (cargo-llvm-cov, cargo-fuzz in flake.nix:22-24)"
  fi
done

# iai-callgrind-runner
if command -v iai-callgrind-runner >/dev/null 2>&1 || nix develop . --command bash -c 'command -v iai-callgrind-runner >/dev/null 2>&1'; then
  skip "iai-callgrind-runner already installed"
else
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would cargo install iai-callgrind-runner --version 0.16.1"
  else
    sub "installing iai-callgrind-runner 0.16.1 (this is quick)…"
    if nix develop . --command bash -c 'cargo install iai-callgrind-runner --version 0.16.1 --locked -q' 2>&1 | tail -5; then
      ok "iai-callgrind-runner installed"
    else
      warn "iai-callgrind-runner install failed (network?), will be retried on next init"
    fi
  fi
fi

# why3 / z3 / cbmc / valgrind (flake)
for bin in why3 z3 cbmc valgrind; do
  if nix develop . --command bash -c "command -v $bin >/dev/null 2>&1"; then
    skip "$bin via flake"
  else
    warn "$bin not in devShell (add to flake.nix if needed)"
  fi
done

# steam-run + cbmc for Kani FHS (the furnace fix)
if nix develop . --command bash -c 'command -v steam-run >/dev/null 2>&1 && command -v cbmc >/dev/null 2>&1'; then
  skip "steam-run + cbmc present (Kani FHS ready)"
else
  warn "steam-run/cbmc missing in devShell — flake.nix should contain pkgs.steam-run + pkgs.cbmc (see Kani FHS comment)"
fi

# why3 config
if [[ -f ~/.why3.conf ]]; then
  skip "why3 config ~/.why3.conf present"
else
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would run: why3 config detect (inside devShell)"
  else
    if nix develop . --command bash -c 'why3 config detect 2>&1 | tail -3'; then
      ok "why3 config detected"
    else
      warn "why3 config detect failed (why3 may still work)"
    fi
  fi
fi

# kani
hdr "Kani — the FHS tale"
printf "  %sKani hardcodes FHS glibc paths and hits the NixOS stub-ld%s\n" "$DIM" "$RESET"
printf "  %sEven a fresh cargo install hits /lib64/ld-linux-x86-64.so.2 → stub.%s\n" "$DIM" "$RESET"
printf "  %sWe run it inside steam-run (FHS bubblewrap) with TMPDIR=/tmp.%s\n" "$DIM" "$RESET"
if command -v cargo-kani >/dev/null 2>&1 || command -v kani >/dev/null 2>&1; then
  skip "cargo-kani/kani already installed (~/.cargo/bin)"
else
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would cargo install --git https://github.com/model-checking/kani kani-verifier --locked"
  else
    sub "installing kani-verifier (this takes a while, ~3 min)…"
    if cargo install --git https://github.com/model-checking/kani kani-verifier --locked 2>&1 | tail -10; then
      ok "kani-verifier installed"
    else
      warn "kani install failed (network?), retry later"
    fi
  fi
fi
# check stale rustup wrapper (the 0l25 bug we patched for keyServ)
if grep -r "0l25mxqza" ~/.rustup 2>/dev/null | grep -q ld.lld; then
  warn "stale rustup gcc-ld wrapper points at old nix store (0l25…), patch needed"
  if [[ "$DRY_RUN" == 0 ]]; then
    sub "patching ~/.rustup ld.lld wrappers to current store…"
    # pure-bash patch without sed: rewrite each file
    cur_wrapper="$(ls /nix/store/*rustup*/nix-support/ld-wrapper.sh 2>/dev/null | head -1 || true)"
    if [[ -n "$cur_wrapper" ]]; then
      # shellcheck disable=SC2044
      for f in $(find ~/.rustup -name "ld.lld" 2>/dev/null); do
        # only patch if contains old hash
        if grep -q "0l25\|xiy3" "$f" 2>/dev/null; then
          tmp="${f}.tmp.$$"
          while IFS= read -r line || [[ -n "$line" ]]; do
            # replace any /nix/store/...-rustup-.../nix-support/ld-wrapper.sh with current
            # bash can't do regex easily, so check for pattern
            if [[ "$line" == *"/nix/store/"*"/nix-support/ld-wrapper.sh"* ]]; then
              printf '"%s" "$@"\n' "$cur_wrapper"
            else
              printf '%s\n' "$line"
            fi
          done < "$f" > "$tmp" && mv "$tmp" "$f"
          chmod +x "$f"
        fi
      done
      ok "patched rustup wrappers → $cur_wrapper"
    else
      warn "no current rustup wrapper found, cannot patch"
    fi
  else
    sub "would patch rustup wrappers"
  fi
else
  skip "rustup wrappers look fresh"
fi

# verify kani via FHS
if [[ "$DRY_RUN" == 0 ]]; then
  if TMPDIR=/tmp nix develop . --command bash -c 'TMPDIR=/tmp steam-run cargo kani --version >/dev/null 2>&1'; then
    ok "kani via steam-run works ($(TMPDIR=/tmp nix develop . --command bash -c 'TMPDIR=/tmp steam-run cargo kani --version 2>&1 | head -1'))"
  else
    warn "kani via steam-run still not working (check cbmc + steam-run in flake, and kani install)"
  fi
else
  sub "would check: TMPDIR=/tmp steam-run cargo kani --version"
fi

# creusot
hdr "Creusot — the shy prover"
if command -v cargo-creusot >/dev/null 2>&1; then
  skip "cargo-creusot already installed"
else
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would cargo install --git https://github.com/creusot-rs/creusot cargo-creusot --locked"
  else
    sub "installing cargo-creusot (this takes a while)…"
    if cargo install --git https://github.com/creusot-rs/creusot cargo-creusot --locked 2>&1 | tail -5; then
      ok "cargo-creusot installed"
    else
      warn "cargo-creusot install failed (network?), retry later"
    fi
  fi
fi
# creusot toolchain at ~/.local/share/creusot
if [[ -x ~/.local/share/creusot/toolchains/nightly-*/bin/creusot-rustc ]] 2>/dev/null || compgen -G "$HOME/.local/share/creusot/toolchains/*/bin/creusot-rustc" >/dev/null 2>&1; then
  skip "creusot toolchain present (~/.local/share/creusot)"
else
  if [[ "$DRY_RUN" == 1 ]]; then
    sub "would run: cargo creusot setup install (downloads nightly toolchain, ~800 MB)"
  else
    warn "creusot toolchain not found at ~/.local/share/creusot — to enable deductive proofs:"
    sub "run: cargo creusot setup install   (or: nix develop --command bash -c 'cargo creusot setup install')"
    sub "then: why3 config detect"
  fi
fi

# -- final verify --
hdr "Final checks"
if [[ "$DRY_RUN" == 0 ]]; then
  if nix develop . --command bash -c 'cargo check --all-targets -q 2>&1 | tail -5'; then
    ok "cargo check --all-targets"
  else
    warn "cargo check failed — fix Cargo names/paths above"
  fi
  if nix develop . --command bash -c 'cargo fmt --check 2>&1 | tail -5'; then
    ok "cargo fmt --check"
  else
    warn "cargo fmt --check failed — run dev-scripts/fmt.sh"
  fi
  # why3
  if nix develop . --command bash -c 'why3 --version >/dev/null 2>&1'; then
    ok "why3 $(nix develop . --command bash -c 'why3 --version 2>&1 | head -1')"
  fi
  if nix develop . --command bash -c 'z3 --version >/dev/null 2>&1'; then
    ok "z3 $(nix develop . --command bash -c 'z3 --version 2>&1 | head -1')"
  fi
else
  sub "would run: cargo check, cargo fmt --check, why3/z3 versions"
fi

# -- closing fun --
hdr "Done — ${PROJECT_TITLE} is ready to misbehave"
printf "  %sProject : %s (%s)%s\n" "$BOLD" "$PROJECT_TITLE" "$PROJECT_SLUG" "$RESET"
printf "  %sAuthor  : %s <%s> %s%s\n" "$BOLD" "$AUTHOR" "$EMAIL" "$YEAR" "$RESET"
printf "  %sLicense : %s%s\n" "$BOLD" "$LICENSE_SPDX" "$RESET"
printf "  %sNext    :%s\n" "$BOLD" "$RESET"
printf "    %s1.%s cargo test && dev-scripts/gate.sh   (gate is red until you add tests by design)\n" "$CYAN" "$RESET"
printf "    %s2.%s dev-scripts/verify.sh               (kani via steam-run, creusot when toolchain installed)\n" "$CYAN" "$RESET"
printf "    %s3.%s dev-scripts/bench.sh --save-baseline=base_v1  (after you write hot paths)\n" "$CYAN" "$RESET"
printf "    %s4.%s git add -A && git commit -m \"%s: initial from template\"%s\n" "$CYAN" "$RESET" "$PROJECT_SLUG" "$RESET"
printf "\n  %sTip: re-run this script anytime — it is idempotent and will only do what is still missing.%s\n" "$DIM" "$RESET"
printf "  %sTemplate philosophy: keep AGENTS.md/DESIGN.md authoritative, keep dev-scripts/ honest.%s\n" "$DIM" "$RESET"
printf "\n  %s🦀 Happy hacking. May your proofs be green and your unwinding be 20.%s\n" "$MAGENTA" "$RESET"

# Exit code: 0 if idempotent, 0 even on partial (resumable). Only hard failures above exit non-zero.
exit 0
