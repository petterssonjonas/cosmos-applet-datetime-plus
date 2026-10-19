import 'recipes/common.just'

name := 'cosmos-applet-datetime-plus'
appid := 'dev.jp.CosmosAppletDatetimePlus'
profile := 'release'
prefix := env('HOME') / '.local'
rootdir := ''
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
base-dir := absolute_path(clean(rootdir / prefix))
config-home := if env('XDG_CONFIG_HOME', '') == '' { env('HOME') / '.config' } else { env('XDG_CONFIG_HOME') }
cache-home := if env('XDG_CACHE_HOME', '') == '' { env('HOME') / '.cache' } else { env('XDG_CACHE_HOME') }

[default]
default: build-release

build-debug:
    cargo build --locked --target-dir {{quote(cargo-target-dir)}}

build-release:
    cargo build --release --locked --target-dir {{quote(cargo-target-dir)}}

check:
    cargo clippy --all-targets --locked --target-dir {{quote(cargo-target-dir)}} -- -D warnings

test:
    cargo test --locked --target-dir {{quote(cargo-target-dir)}}

# Notification reception requires the installed applet to launch through the COSMIC panel.
[positional-arguments]
run *args:
    @printf '%s\n' 'Notification reception requires panel launch with X-NotificationsApplet=true.'
    cargo run --profile {{quote(if profile == 'debug' { 'dev' } else { profile })}} --locked --target-dir {{quote(cargo-target-dir)}} -- "$@"

# Build separately. Atomic replacement is safe while an old executable is running.
install:
    @just --justfile {{quote(justfile())}} _install-file {{quote(cargo-target-dir / profile / name)}} {{quote(base-dir / 'bin' / name)}} 0755
    @just --justfile {{quote(justfile())}} _install-desktop resources/app.desktop {{quote(base-dir / 'share/applications' / appid + '.desktop')}} {{quote(absolute_path(prefix / 'bin' / name))}}
    @just --justfile {{quote(justfile())}} _install-file resources/icon.svg {{quote(base-dir / 'share/icons/hicolor/scalable/apps' / appid + '-symbolic.svg')}} 0644
    @just --justfile {{quote(justfile())}} _install-file resources/places-source.txt {{quote(base-dir / 'share/doc' / name / 'places-source.txt')}} 0644

# Remove the panel entry first. Preferences, agenda and shared cache remain untouched.
uninstall:
    rm -f -- {{quote(base-dir / 'bin' / name)}} {{quote(base-dir / 'share/applications' / appid + '.desktop')}} {{quote(base-dir / 'share/icons/hicolor/scalable/apps' / appid + '-symbolic.svg')}} {{quote(base-dir / 'share/doc' / name / 'places-source.txt')}}

# Stop both applications and every reminder window first. Legacy directories remain backups.
# Use `just migrate false` if the companion already migrated the shared weather cache.
migrate shared-cache='true':
    #!/usr/bin/env bash
    set -euo pipefail
    case {{quote(shared-cache)}} in true|false) ;; *) printf '%s\n' 'shared-cache must be true or false.' >&2; exit 1 ;; esac
    just --justfile {{quote(justfile())}} _migrate-directory {{quote(config-home / 'cosmic-applet-datetime-plus')}} {{quote(config-home / name)}}
    if [[ {{quote(shared-cache)}} == true ]]; then
        just --justfile {{quote(justfile())}} _migrate-directory {{quote(cache-home / 'cosmic-applet-datetime-plus')}} {{quote(cache-home / name)}}
    fi
