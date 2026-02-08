#!/bin/env sh
set -eu

usage() {
  echo "Usage: $(basename "$0") <version>"
}

if [ "$#" -lt 1 ]; then
  usage
  exit 1
fi

version="$1"
tag="v${version}"
notes_file="release_notes/${version}.md"

if [ ! -f "$notes_file" ]; then
  echo "Release notes not found: $notes_file"
  exit 1
fi

status="$(git status --porcelain)"
if [ -n "$status" ]; then
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    path="${line#???}"
    if [ "$path" != "$notes_file" ]; then
      echo "Working tree must be clean (only ${notes_file} may be dirty)."
      echo "$status"
      exit 1
    fi
  done <<EOF
$status
EOF
fi

update_package_version() {
  file="$1"
  tmp="${file}.tmp.$$"
  if awk -v v="$version" '
    BEGIN { in_pkg = 0; done = 0 }
    /^\[.*\]/ {
      in_pkg = ($0 == "[package]")
      print
      next
    }
    in_pkg && !done && $0 ~ /^[[:space:]]*version[[:space:]]*=/ {
      sub(/version[[:space:]]*=[[:space:]]*"[^"]*"/, "version = \"" v "\"")
      done = 1
    }
    { print }
    END { if (!done) exit 2 }
  ' "$file" > "$tmp"; then
    mv "$tmp" "$file"
  else
    rm -f "$tmp"
    echo "Missing package version in $file"
    exit 1
  fi
}

update_root_dependencies() {
  file="$1"
  tmp="${file}.tmp.$$"
  if awk -v v="^${version}" '
    {
      if ($0 ~ /lumalla_[^=]*=.*version[[:space:]]*=/) {
        gsub(/version[[:space:]]*=[[:space:]]*"[^"]*"/, "version = \"" v "\"")
      }
      print
    }
  ' "$file" > "$tmp"; then
    mv "$tmp" "$file"
  else
    rm -f "$tmp"
    echo "Failed to update root dependencies in $file"
    exit 1
  fi
}

for manifest in $(rg --files -g 'Cargo.toml' -g '!**/target/**'); do
  update_package_version "$manifest"
done

update_root_dependencies "Cargo.toml"

cargo generate-lockfile

git add Cargo.toml crates/*/Cargo.toml Cargo.lock "$notes_file"
git commit -m "Release ${version}"

gh release create "$tag" -F "$notes_file" --title "$tag"
