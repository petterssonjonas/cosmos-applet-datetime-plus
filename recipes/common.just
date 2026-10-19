# Shared Cosmos family installation primitives. Keep identical across repositories.
# Public recipes and application-specific file lists live in the root justfile.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Atomic replacement allows upgrading an executable while its old inode is running.
_install-file source destination mode:
    #!/usr/bin/env bash
    set -euo pipefail
    destination={{quote(destination)}}
    mkdir -p -- "$(dirname -- "$destination")"
    temporary=$(mktemp "${destination}.XXXXXX")
    trap 'rm -f -- "$temporary"' EXIT
    install -m {{quote(mode)}} -- {{quote(source)}} "$temporary"
    mv -fT -- "$temporary" "$destination"

# A desktop entry needs both Exec argument escaping and desktop string escaping.
_install-desktop source destination executable:
    #!/usr/bin/env bash
    set -euo pipefail
    destination={{quote(destination)}}
    executable={{quote(executable)}}
    if [[ "$executable" == *$'\n'* || "$executable" == *$'\r'* ]]; then
        printf '%s\n' 'Executable paths must not contain newlines.' >&2
        exit 1
    fi
    mkdir -p -- "$(dirname -- "$destination")"
    temporary=$(mktemp "${destination}.XXXXXX.desktop")
    trap 'rm -f -- "$temporary"' EXIT
    install -m 0644 -- {{quote(source)}} "$temporary"
    executable=${executable//%/%%}
    executable=${executable//\\/\\\\}
    executable=${executable//\"/\\\"}
    executable=${executable//\`/\\\`}
    executable=${executable//\$/\\\$}
    # desktop-file-edit performs the desktop-entry string escaping itself.
    desktop-file-edit --set-key=Exec --set-value="\"$executable\"" "$temporary"
    desktop-file-validate "$temporary"
    mv -fT -- "$temporary" "$destination"

# Explicit, non-destructive migration: never merge over newer preferences or secrets.
_migrate-directory source destination:
    #!/usr/bin/env bash
    set -euo pipefail
    source={{quote(source)}}
    destination={{quote(destination)}}
    [[ "$source" == /* && "$destination" == /* ]] || { printf '%s\n' 'Migration paths must be absolute.' >&2; exit 1; }
    check_ancestors() {
        local path=$1 owner mode
        while [[ "$path" != / ]]; do
            if [[ -e "$path" || -L "$path" ]]; then
                [[ -d "$path" && ! -L "$path" ]] || { printf 'Unsafe migration ancestor: %s\n' "$path" >&2; return 1; }
                owner=$(stat -c %u -- "$path")
                mode=$(stat -c %a -- "$path")
                [[ "$owner" == 0 || "$owner" == "$(id -u)" ]] || { printf 'Foreign-owned migration ancestor: %s\n' "$path" >&2; return 1; }
                # Root-owned sticky temporary directories are safe for owned children.
                (( (8#$mode & 0022) == 0 || (owner == 0 && (8#$mode & 01000) != 0) )) || { printf 'Writable migration ancestor: %s\n' "$path" >&2; return 1; }
            fi
            path=$(dirname -- "$path")
        done
    }
    [[ -e "$source" || -L "$source" ]] || exit 0
    [[ -d "$source" && ! -L "$source" && $(stat -c %u -- "$source") == $(id -u) ]] || { printf '%s\n' 'Legacy data must be an owned, real directory.' >&2; exit 1; }
    [[ ! -e "$destination" && ! -L "$destination" ]] || { printf 'Migration destination already exists: %s\n' "$destination" >&2; exit 1; }
    check_ancestors "$(dirname -- "$source")"
    check_ancestors "$(dirname -- "$destination")"
    [[ -z $(find -P "$source" \( ! -uid "$(id -u)" -o ! \( -type f -o -type d \) -o -perm /0022 \) -print -quit) ]] || { printf '%s\n' 'Legacy data must contain only owned, non-writable-by-others regular files and directories; stop old apps and remove only confirmed stale sockets first.' >&2; exit 1; }
    mkdir -p -- "$(dirname -- "$destination")"
    check_ancestors "$(dirname -- "$destination")"
    temporary=$(mktemp -d "${destination}.XXXXXX")
    trap 'rm -rf -- "$temporary"' EXIT
    cp -a -- "$source/." "$temporary/"
    chmod 0700 -- "$temporary"
    mv -T --no-clobber -- "$temporary" "$destination"
    [[ ! -e "$temporary" ]] || { printf '%s\n' 'Migration destination appeared concurrently; nothing overwritten.' >&2; exit 1; }
    printf 'Copied %s to %s; legacy data retained.\n' "$source" "$destination"
