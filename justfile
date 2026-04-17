name := `grep -m 1 -oP '(?<=<binary>).*?(?=</binary>)' $(ls ./res/*.xml | head -n 1)`
id := `grep -m 1 -oP '(?<=<id>).*?(?=</id>)' $(ls ./res/*.xml | head -n 1)`

export APPID := id

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))

export INSTALL_DIR := base-dir / 'share'

bin-src := 'target' / 'release' / name
bin-dst := base-dir / 'bin' / name

desktop := APPID + '.desktop'
desktop-src := 'res' / desktop
desktop-dst := clean(rootdir / prefix) / 'share' / 'applications' / desktop

metainfo := APPID + '.metainfo.xml'
metainfo-src := 'res' / metainfo
metainfo-dst := clean(rootdir / prefix) / 'share' / 'metainfo' / metainfo

icons-src := 'res' / 'icons'
icons-dst := clean(rootdir / prefix) / 'share' / 'icons' / 'hicolor' / 'scalable'

default: build-release

# Compiles with debug profile
build-debug *args:
    cargo build {{args}}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Runs a clippy check
check *args:
    cargo clippy --all-features {{args}} -- -W clippy::pedantic

# Format and run
dev *args:
    cargo fmt
    just run {{args}}

# Run with debug logs
run *args:
    env RUST_LOG=day=info RUST_BACKTRACE=full cargo run {{args}}

# Installs files
install:
    strip {{bin-src}}
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0644 {{desktop-src}} {{desktop-dst}}
    install -Dm0644 {{metainfo-src}} {{metainfo-dst}}
    for svg in {{icons-src}}/scalable/apps/*.svg; do \
        install -D "$svg" "{{icons-dst}}/apps/$(basename $svg)"; \
    done

# Uninstalls installed files
uninstall:
    rm {{bin-dst}}
    rm {{desktop-dst}}
    rm {{metainfo-dst}}
    for svg in {{icons-src}}/scalable/apps/*.svg; do \
        rm "{{icons-dst}}/apps/$(basename $svg)"; \
    done

# Runs `cargo clean`
clean:
    cargo clean

# Generate cargo-sources.json for the Flatpak offline build
flatpak-cargo-sources:
    #!/usr/bin/env bash
    set -e
    # pop-os force-pushes iced; don't let a stale cached clone's submodule pointer
    # break `git fetch` (the generator runs `git submodule update` itself anyway).
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=fetch.recurseSubmodules GIT_CONFIG_VALUE_0=false
    if [ ! -f flatpak-cargo-generator.py ]; then
        curl -fLo flatpak-cargo-generator.py \
            https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
    fi
    if python3 -c "import aiohttp, tomlkit" 2>/dev/null; then
        python3 flatpak-cargo-generator.py ./Cargo.lock -o cargo-sources.json
    else
        if [ ! -d .flatpak-venv ] || ! .flatpak-venv/bin/python3 --version &>/dev/null; then
            rm -rf .flatpak-venv
            python3 -m venv .flatpak-venv
        fi
        .flatpak-venv/bin/pip install --quiet aiohttp tomlkit
        .flatpak-venv/bin/python flatpak-cargo-generator.py ./Cargo.lock -o cargo-sources.json
    fi

# Build and install the Flatpak for the current user
flatpak-build: flatpak-cargo-sources
    #!/usr/bin/env bash
    set -e
    # Shadow appstreamcli with a no-op: the local one segfaults in `compose`
    # (same workaround as memaker). CI/cosmic-flatpak builds run the real one.
    tmpdir=$(mktemp -d)
    printf '#!/bin/sh\nexit 0\n' > "$tmpdir/appstreamcli"
    chmod +x "$tmpdir/appstreamcli"
    PATH="$tmpdir:$PATH" flatpak-builder --user --install --force-clean --delete-build-dirs --disable-rofiles-fuse build-dir {{id}}.yml
    rm -rf "$tmpdir"

# Remove the installed Flatpak
flatpak-uninstall:
    flatpak uninstall --user -y {{id}} || true
