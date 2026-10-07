{
  description = "Project development environment (vigOS toolchain).";

  # Downstream repos consume the shared toolchain as a flake INPUT, so updating
  # the dev environment means bumping that input — it never overwrites your
  # files. To update: `nix flake update vigos`.
  inputs = {
    # The shared vigOS toolchain (single source of truth).
    # Pinned to a devkit release tag (policy: https://github.com/vig-os/devkit/blob/main/docs/NIX.md,
    # "Home-manager modules - versioning & release policy"). The pin must match
    # DEVKIT_VERSION in .vig-os; with DEVKIT_FLAKE_PIN_ADVANCE=true, devkit
    # upgrades (`install.sh --force`) advance it together with flake.lock.
    vigos.url = "github:vig-os/devkit?ref=1.18.0";
    # Follow vigos's pinned nixpkgs + flake-utils so your tools match the
    # toolchain exactly (one resolved nixpkgs, no drift).
    nixpkgs.follows = "vigos/nixpkgs";
    flake-utils.follows = "vigos/flake-utils";
  };

  outputs =
    {
      self,
      vigos,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ vigos.overlays.default ];
          config.allowUnfree = true;
        };

        # ────────────────────────────────────────────────────────────────────
        # Your project tools go here. This block is YOURS: a dev-environment
        # update never overwrites it (scaffold-once / never-overwrite, the same
        # guarantee as justfile.project and docker-compose.project.yaml).
        #
        #   extraPackages = pkgs: [
        #     pkgs.postgresql_16
        #     pkgs.ffmpeg
        #   ];
        # ────────────────────────────────────────────────────────────────────
        extraPackages = pkgs: [
          # add project tools here (the Rust toolchain comes from `rust`)
        ];

        # The Rust pack (vigos.lib.mkRustProject): the toolchain pinned by
        # rust-toolchain.toml (fenix), crane-built checks (clippy, fmt,
        # nextest, doctest, doc) and the cargo-auditable package. Its own
        # devShell is not used: it does not forward the .vig-os knobs yet
        # (vig-os/devkit#1810), so the dev shell below is mkProjectShell with
        # the same toolchain and the knobs.
        rust = vigos.lib.mkRustProject {
          inherit pkgs;
          src = ./.;
          toolchainHash = "sha256-gh/xTkxKHL4eiRXzWv8KP7vfjSk61Iq48x47BEDFgfk=";
          # include_str!'d by src/event.rs; not a cargo source file.
          extraSrcFiles = [
            "event.schema.json"
            "docs/agent-guide.md"
          ];
          # tests/tty.rs reads the terminal's foreground group with `ps`.
          nativeBuildInputs = [ pkgs.procps ];
        };

        # Devkit knobs read from .vig-os (#1224, #1432, #1431, #1282, #1633): the
        # flake-generated pre-commit hooks — the branch guard and the
        # commit-message validator — follow the workspace manifest, mirroring
        # the scaffolded .pre-commit-config.yaml renders (#1434). Managed
        # block; leave it.
        vigOsValue =
          key:
          let
            vigOsPath = self + "/.vig-os";
            declared = builtins.filter (l: nixpkgs.lib.hasPrefix "${key}=" l) (
              nixpkgs.lib.splitString "\n" (builtins.readFile vigOsPath)
            );
          in
          if !builtins.pathExists vigOsPath || declared == [ ] then
            ""
          else
            nixpkgs.lib.removePrefix "${key}=" (builtins.head declared);

        # A comma-separated manifest list -> a Nix list, or null when the key
        # is absent/blank (= "keep the devkit default"). Whitespace around
        # entries is trimmed and empty entries dropped, matching how
        # init-workspace.sh resolves the same keys; validation (charset,
        # non-empty) lives in mkProjectShell, which fails eval loudly on a bad
        # value.
        vigOsList =
          key:
          let
            entries = builtins.filter (t: t != "") (
              map (t: nixpkgs.lib.trim t) (nixpkgs.lib.splitString "," (vigOsValue key))
            );
          in
          if entries == [ ] then null else entries;

        # Workflow model (#1224): a `trunk` workspace drops the dev-branch
        # clause. `gitflow` (the default) and an absent/blank value are inert.
        workflow = if vigOsValue "DEVKIT_WORKFLOW" == "trunk" then "trunk" else "gitflow";

        # Branch-type set (#1432): DEVKIT_BRANCH_TYPES replaces the
        # issue-numbered alternation of the branch guard.
        branchTypes = vigOsList "DEVKIT_BRANCH_TYPES";

        # Approved commit types (#1431): DEVKIT_COMMIT_TYPES replaces the
        # validate-commit-msg `--types` list, so the local hook agrees with
        # CI's validate-commit-range (#1434).
        commitTypes = vigOsList "DEVKIT_COMMIT_TYPES";

        # Refs policy (#1282): DEVKIT_REFS_POLICY steers whether a commit needs
        # a `Refs: #N` line — chore-optional (default) | optional | required.
        # Absent/blank forwards null (= the default); an unknown literal fails
        # eval loudly in mkProjectShell (#1434).
        refsPolicy =
          let
            raw = nixpkgs.lib.trim (vigOsValue "DEVKIT_REFS_POLICY");
          in
          if raw == "" then null else raw;

        # Refs-optional types (#1633): DEVKIT_REFS_OPTIONAL_TYPES names the
        # commit types that may omit `Refs:` and WINS over DEVKIT_REFS_POLICY.
        # Absent/blank forwards null (= the policy decides); a value outside
        # the approved types fails eval loudly in mkProjectShell.
        refsOptionalTypes = vigOsList "DEVKIT_REFS_OPTIONAL_TYPES";
      in
      {
        # The dev shell = the shared vigOS toolchain + your extras.
        # `direnv allow` (via .envrc) or `nix develop` enters it.
        devShells.default = vigos.lib.mkProjectShell (
          {
            inherit pkgs;
            extraPackages = extraPackages pkgs;
            # The rust capability module, fed the same toolchain the checks
            # build with (what mkRustProject's own devShell would wire).
            modules = [
              {
                name = "rust";
                checks = "mkRustProject";
                inherit (rust) toolchain;
              }
            ];

            # Host-runner hooks (#1167): direnv CI runs on the bare host
            # runner, so let the flake GENERATE .pre-commit-config.yaml from
            # the shared base hook set, resolved entirely from the Nix store
            # (incl. pymarkdown, now a flake system hook, #1170) rather than
            # building the committed YAML remote pre-commit repo hook envs
            # per runner. Customize like the opt-in block below; the generated
            # config is a gitignored /nix/store symlink.
            hooks = {
              # Rust gates, run by `just precommit` (and so by CI's lint job).
              cargo-fmt = {
                enable = true;
                name = "cargo fmt --check";
                entry = "cargo fmt --all -- --check";
                files = "\\.rs$";
                language = "system";
                pass_filenames = false;
              };
              cargo-clippy = {
                enable = true;
                name = "cargo clippy -D warnings";
                entry = "cargo clippy --all-targets --locked -- -D warnings";
                files = "(\\.rs$|^Cargo\\.(toml|lock)$)";
                language = "system";
                pass_filenames = false;
              };
            };

            # Opt-in: let the flake GENERATE .pre-commit-config.yaml from the
            # shared base hook set instead of hand-managing the scaffolded
            # YAML — toggle base hooks, add per-hook/global excludes, or add
            # fully custom hooks; hook updates then flow with `nix flake
            # update vigos`, and your customization lives HERE (preserved).
            # Contract + migration steps:
            # https://github.com/vig-os/devkit/blob/main/docs/MIGRATION.md ("Customizing
            # pre-commit hooks from the project flake"). Uncomment to opt in, then
            # delete .pre-commit-config.yaml (the generated config refuses to
            # overwrite an existing file). The generated store symlink is ignored
            # automatically on (re)scaffold (#1092); add durable root ignores you
            # own to .gitignore.project.
            #
            #   hooks = {
            #     typos.enable = false;                    # toggle a base hook
            #     detect-private-keys.excludes = [ "worker/src/index\\.ts" ];
            #     my-data-check = {                        # fully custom hook
            #       enable = true;
            #       entry = "./scripts/check-dat.sh";
            #       files = "\\.dat$";
            #       language = "system";
            #     };
            #   };
            #   hooksExcludes = [ "^data/stopping/" "\\.dat$" ]; # global excludes
          }
          # Forwarded only when the resolved devkit accepts it (#1249): the vigos
          # input floats to main, which may predate the argument; older builders
          # then fall back to their gitflow default instead of failing eval.
          // nixpkgs.lib.optionalAttrs (builtins.functionArgs vigos.lib.mkProjectShell ? workflow) {
            # Branch guard follows the workspace workflow model (#1224).
            inherit workflow;
          }
          // nixpkgs.lib.optionalAttrs (builtins.functionArgs vigos.lib.mkProjectShell ? branchTypes) {
            # Branch guard follows the workspace branch-type set (#1432).
            inherit branchTypes;
          }
          // nixpkgs.lib.optionalAttrs (builtins.functionArgs vigos.lib.mkProjectShell ? commitTypes) {
            # validate-commit-msg follows the workspace commit-type set (#1431).
            inherit commitTypes;
          }
          // nixpkgs.lib.optionalAttrs (builtins.functionArgs vigos.lib.mkProjectShell ? refsPolicy) {
            # validate-commit-msg follows the workspace Refs policy (#1282).
            inherit refsPolicy;
          }
          // nixpkgs.lib.optionalAttrs (builtins.functionArgs vigos.lib.mkProjectShell ? refsOptionalTypes) {
            # validate-commit-msg follows the workspace exempt set (#1633).
            inherit refsOptionalTypes;
          }
        );

        inherit (rust) checks;
        packages = rust.packages // {
          watcher-s1 = rust.packages.default;
        };

        apps.default = {
          type = "app";
          program = "${rust.packages.default}/bin/watcher-s1";
          meta.description = "Truthful command wrapper that detects stalls, prompts and masked failures";
        };

        # Opt-in local dev services (#795): a daemonless process-compose stack
        # (Postgres, SeaweedFS/S3, Redis, …) with service versions from the
        # pinned vigos nixpkgs — no Docker/Podman daemon, no extra flake
        # inputs. Uncomment, then `nix run .#services` (or enable the
        # `services` recipe in justfile.project); service state lands in
        # ./data — add it to .gitignore.
        #
        #   packages.services = vigos.lib.mkProjectServices {
        #     inherit pkgs;
        #     modules = [ { services.postgres."db".enable = true; } ];
        #   };

        # Future (upstream, opt-in): vigos may expose modular language shells —
        # e.g. `vigos.devShells.${system}.{cpp,geant4,dataAnalysis}` — that you
        # select without changing this scaffold. Out of scope today.
      }
    );
}
