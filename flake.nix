{
  description = "tilt-devenv — manage a multi-repo dev environment as one unit, live in Tilt";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin"];

      imports = [inputs.git-hooks-nix.flakeModule];

      perSystem = {
        pkgs,
        system,
        config,
        ...
      }: let
        # `repos` — the cross-repo branch tool. A Rust workspace (repos-core lib +
        # the `repos` CLI + the `repos-tiltd` daemon) built with crane +
        # rust-overlay. crane vendors from Cargo.lock, so there is no
        # vendorHash/cargoHash to maintain.
        #
        # This repo IS the tool: the workspace lives at the repo root, so the
        # crane source and the pre-commit wrappers point at ./ (not ./tools like
        # the dev-env that imports this flake).
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
        # The default Cargo source filter keeps only Rust and Cargo files, which
        # would drop the lnav format `repos-core` embeds with include_str!.
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            ./crates/repos-core/src/logs_format.json
          ];
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
        };

        # Cached dependency layer, reused by the package build and the checks.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Vendored deps so the offline clippy pre-commit hook can resolve crates
        # without network — both at `git commit` time AND inside the network-less
        # `nix flake check` sandbox.
        cargoVendorDir = craneLib.vendorCargoDeps {inherit src;};

        repos = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            pname = "repos";
            # Build both frontend binaries (the CLI and the daemon); repos-core is
            # a library, pulled in as their dependency.
            cargoExtraArgs = "-p repos -p repos-tiltd";
            # Tests spawn git to build throwaway repos, lnav to assert the merged
            # log order, and ps to walk up to the Tilt a daemon runs under.
            nativeBuildInputs = [pkgs.installShellFiles pkgs.git pkgs.lnav pkgs.procps];
            # Completions so an importing devShell picks them up. The extension
            # ships beside the binary — `../share/repos/tilt/Tiltfile` from
            # `command -v repos` — so one PATH lookup resolves both, and the
            # extension can never drift from the binaries it drives.
            postInstall = ''
              installShellCompletion --cmd repos \
                --bash <(COMPLETE=bash $out/bin/repos) \
                --zsh <(COMPLETE=zsh $out/bin/repos) \
                --fish <(COMPLETE=fish $out/bin/repos)
              install -Dm644 ${./tilt/Tiltfile} $out/share/repos/tilt/Tiltfile
            '';
            meta.mainProgram = "repos";
          });

        # The Tilt extension is Starlark, not Rust, so it needs its own check:
        # `tests/tilt/run.sh` evaluates the extension end to end with the built
        # `repos` binary and asserts the result. Only the files the harness needs
        # (the extension + the test tree) go into the sandbox.
        tiltTestSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [./tilt ./tests];
        };
        tiltfileTest =
          pkgs.runCommandLocal "tiltfile-test" {
            nativeBuildInputs = [repos pkgs.tilt pkgs.git pkgs.python3 pkgs.bash];
            # Keep Tilt offline and non-interactive in the build sandbox.
            env = {
              DO_NOT_TRACK = "1";
              TILT_DISABLE_ANALYTICS = "1";
            };
          } ''
            cp -r ${tiltTestSrc}/. work
            chmod -R u+w work
            cd work
            bash tests/tilt/run.sh
            touch $out
          '';
      in {
        _module.args.pkgs = import inputs.nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [inputs.rust-overlay.overlays.default];
        };

        packages = {
          default = repos;
          repos = repos;
        };

        checks = {
          # `repos` runs the workspace tests during its own build (doCheck
          # defaults to true), so this check exercises them too.
          inherit repos;
          tiltfile-test = tiltfileTest;
        };

        formatter = pkgs.alejandra;

        pre-commit.settings.hooks = {
          alejandra.enable = true;
          rustfmt-tool = {
            enable = true;
            entry = pkgs.lib.getExe (pkgs.writeShellApplication {
              name = "cargo-fmt-check";
              runtimeInputs = [rustToolchain];
              text = "exec cargo fmt --check";
            });
            files = "\\.rs$";
            pass_filenames = false;
          };
          clippy-tool = {
            enable = true;
            entry = pkgs.lib.getExe (pkgs.writeShellApplication {
              name = "cargo-clippy-offline";
              runtimeInputs = [rustToolchain];
              text = ''
                CARGO_HOME=$(mktemp -d)
                cp ${cargoVendorDir}/config.toml "$CARGO_HOME/config.toml"
                export CARGO_HOME
                exec cargo-clippy clippy --all-targets --offline "$@" -- --deny warnings
              '';
            });
            files = "\\.rs$";
            pass_filenames = false;
          };
          cargo-test-tool = {
            enable = true;
            entry = pkgs.lib.getExe (pkgs.writeShellApplication {
              name = "cargo-test-offline";
              # Tests spawn git to build throwaway repos, and lnav to assert the
              # merged log order.
              runtimeInputs = [rustToolchain pkgs.git pkgs.lnav];
              text = ''
                CARGO_HOME=$(mktemp -d)
                cp ${cargoVendorDir}/config.toml "$CARGO_HOME/config.toml"
                export CARGO_HOME
                exec cargo test --workspace --offline "$@"
              '';
            });
            files = "\\.rs$";
            pass_filenames = false;
          };
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            git
            rust-analyzer
            cargo-watch
            just
            # For exercising the Tilt extension against examples/.
            tilt
            lnav
            # `just icons` rasterises the nav button's SVG to look at it.
            librsvg
            # Both binaries on PATH so the example Tiltfile can call them.
            config.packages.repos
          ];
          shellHook = config.pre-commit.installationScript;
        };
      };
    };
}
