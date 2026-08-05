default:
    @just --list

# Bootstrap throwaway local repos + tilt-devenv.json (offline, safe to rerun), then `tilt up`
demo:
    examples/bootstrap.sh
    cd examples && tilt up

# Like `demo`, but every repos/repos-tiltd call runs `cargo run` from source instead of the Nix-packaged binaries
dev:
    dev/bootstrap.sh
    cd dev && tilt up
