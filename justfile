default:
    @just --list

# Bootstrap throwaway local repos + tilt-devenv.json (offline, safe to rerun), then `repos up`
demo:
    examples/bootstrap.sh
    cd examples && repos up

# Like `demo`, but every repos/repos-tiltd call runs `cargo run` from source instead of the Nix-packaged binaries
dev:
    dev/bootstrap.sh
    cd dev && cargo run --manifest-path ../Cargo.toml -p repos -- up

# Drive the dev-env update button by hand: a throwaway dev environment one commit behind its own origin
update-demo:
    dev/update-demo.sh
    cd dev/.update-demo/env && cargo run --manifest-path ../../../Cargo.toml -p repos -- up

# The same rig under a bare `tilt up`: no supervisor, so the button pulls but can't restart
update-demo-bare:
    dev/update-demo.sh
    cd dev/.update-demo/env && tilt up

# Render the update button's icon at each badge width (1, 42, 999) to eyeball an icon change
icons:
    mkdir -p target/icons
    cargo test -p repos-tiltd dump_update_icons -- --ignored --nocapture
    for n in 1 42 999; do \
      rsvg-convert -h 160 -b '#1e2020' target/icons/update-$n.svg -o target/icons/update-$n.png; \
      rsvg-convert -h 24  -b '#1e2020' target/icons/update-$n.svg -o target/icons/update-$n-actual-size.png; \
    done
    @echo "Wrote target/icons/update-{1,42,999}[-actual-size].png"
