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
    env RUST_LOG=cosmic_applet_time=info RUST_BACKTRACE=full cargo run {{args}}

# Installs files
install:
    strip {{bin-src}}
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0644 {{desktop-src}} {{desktop-dst}}
    install -Dm0644 {{metainfo-src}} {{metainfo-dst}}
    for svg in {{icons-src}}/apps/*.svg; do \
        install -D "$svg" "{{icons-dst}}/apps/$(basename $svg)"; \
    done

# Uninstalls installed files
uninstall:
    rm {{bin-dst}}
    rm {{desktop-dst}}
    rm {{metainfo-dst}}
    for svg in {{icons-src}}/apps/*.svg; do \
        rm "{{icons-dst}}/apps/$(basename $svg)"; \
    done

# Runs `cargo clean`
clean:
    cargo clean
