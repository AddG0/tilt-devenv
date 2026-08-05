default:
    @just --list

# Bootstrap throwaway local repos + tilt-devenv.json (offline, safe to rerun), then `tilt up`
demo:
    examples/bootstrap.sh
    cd examples && tilt up

# Like `demo`, but repos-branches runs `cargo run -p repos-tiltd` from source instead of the Nix-packaged binary
dev:
    dev/bootstrap.sh
    cd dev && tilt up
