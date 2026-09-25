smallbuild:
    cargo build --release
    
build: fmt lint test
    cargo build --release

install:
    cargo install --path helix-term --locked

local_build_install: smallbuild install
    echo "Build and install complete!"

    
build_and_install: build install
    echo "Build and install complete!"

test: unit-test integration-test

unit-test:
    cargo test --workspace --lib

integration-test:
    cargo test --workspace --tests --features integration

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

fmt:
    cargo fmt --all

profile_out := "target/profiling/dhx-profile.json.gz"
cdb := env_var_or_default("CDB", "C:/Program Files (x86)/Windows Kits/10/Debuggers/x64/cdb.exe")

# Record a CPU profile of dhx with symbols and print its hotspots: just profile README.md
profile *args:
    cargo build --profile profiling --bin dhx
    samply record --save-only --unstable-presymbolicate -o {{profile_out}} -- target/profiling/dhx {{args}}
    python scripts/samply_top.py {{profile_out}}

# Open the last profile in the Firefox profiler.
profile-view:
    samply load {{profile_out}}

# Print every thread's stack of a running dhx without stopping it (hangs, stalls).
[windows]
stacks pid:
    "{{cdb}}" -pv -p {{pid}} -c "~*k 40; qd"
