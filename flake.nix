{
  description = "Mirri Code";

  inputs = {
    # Pinned to the 25.11 release channel for the general package set. Node.js
    # itself does NOT come from nixpkgs — see `nodejsFor` below: the JS
    # toolchain uses the official Node binaries pinned to the security floor,
    # because nixpkgs' `nodejs_24` lags behind it and `.npmrc`'s
    # `engine-strict=true` makes `pnpm install` reject anything older.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          f (import nixpkgs {
            inherit system;
          })
        );

      # Security floor for every run of the JS toolchain (web SPA build and
      # the engine-strict `pnpm install` inside fetchPnpmDeps), including the
      # Nix sandbox itself. nixpkgs' `nodejs_24` lags this floor on every
      # channel today, so the toolchain consumes the official Node binaries,
      # pinned per platform with their published SHASUMS256 values.
      nodeVersion = "24.22.2";

      nodePlatform = {
        "x86_64-linux" = "linux-x64";
        "aarch64-linux" = "linux-arm64";
        "x86_64-darwin" = "darwin-x64";
        "aarch64-darwin" = "darwin-arm64";
      };

      # SRI hashes from https://nodejs.org/dist/v24.22.2/SHASUMS256.txt
      nodeHashes = {
        "x86_64-linux" = "sha256-SMwOIeBIeQ5UwM9XScJyUAUxPWUmUcQLeiH+uWl6r3U=";
        "aarch64-linux" = "sha256-6xh4N6IjDUBHFhrAvgCFhOFrRvcm6clJDuNdxrF9m6g=";
        "x86_64-darwin" = "sha256-aWVXj9gj5ywbU4f1q4TLJTaPiG4C64d55vD85rkcJ7c=";
        "aarch64-darwin" = "sha256-H1rAdb5QvtR4DqO+GaAH/alepNUn0o120Lkd5H2RURU=";
      };

      nodejsFor =
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
          platform = nodePlatform.${system};
        in
        pkgs.stdenv.mkDerivation {
          pname = "nodejs";
          version = nodeVersion;
          src = pkgs.fetchurl {
            url = "https://nodejs.org/dist/v${nodeVersion}/node-v${nodeVersion}-${platform}.tar.xz";
            sha256 = nodeHashes.${system};
          };
          # Official binaries are dynamically linked glibc ELFs; patch the
          # interpreter and library paths in place on Linux. Darwin binaries
          # are natively runnable and need no patching.
          nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.autoPatchelfHook
          ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.stdenv.cc.cc.lib
          ];
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r bin include lib share $out/
            runHook postInstall
          '';
        };

      pnpmFor =
        pkgs:
        pkgs.pnpm_10.override {
          nodejs = nodejsFor pkgs;
        };

      # -------------------------------------------------------------------
      # Workspace members (kept in sync with pnpm-workspace.yaml).
      #
      # HARD REQUIREMENT: whenever you add or remove a workspace package,
      # you MUST update both lists below. Missing a path will break the Nix
      # build (src fileset silently drops files); missing a name will break
      # pnpmConfigHook (dependencies for that workspace won't be fetched).
      # -------------------------------------------------------------------
      workspacePaths = [
        ./packages/e2e-harness
        ./packages/server-e2e
        ./packages/protocol
        ./packages/release
        ./apps/mirri-desktop
        ./apps/mirri-web
        ./docs
      ];

      workspaceNames = [
        "@mirri-ai/e2e-harness"
        "@mirri-ai/mirri-desktop"
        "@mirri-ai/mirri-web"
        "@mirri-ai/protocol"
        "@mirri-ai/release"
        "mirri-code-docs"
      ];
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          nodejs = nodejsFor pkgs;
          pnpm = pnpmFor pkgs;
          # The rust workspace is the shipped engine, so the anchor package is
          # the only version source (`rust/scripts/sync-version.mjs` keeps the
          # rust workspace on it; the CI lint job asserts they match).
          releasePackageJson = builtins.fromJSON (builtins.readFile ./packages/release/package.json);
          version = releasePackageJson.version;

          # The workspace the JS toolchain needs — only to compile the web SPA
          # that the rust binary embeds (rust-embed).
          jsSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions (
              [
                ./.npmrc
                ./.nvmrc
                ./package.json
                ./pnpm-lock.yaml
                ./pnpm-workspace.yaml
                ./tsconfig.json
                ./LICENSE
              ]
              ++ workspacePaths
            );
          };

          # The rust workspace alone. Local build outputs must never leak in:
          # `target/` is the developer's own build tree and `web-dist/` is a
          # staging directory this derivation populates itself.
          rustSrc = lib.fileset.toSource {
            root = ./rust;
            fileset = lib.fileset.difference ./rust (
              lib.fileset.unions [
                (lib.fileset.maybeMissing ./rust/target)
                ./rust/crates/mirri-server/web-dist
              ]
            );
          };

          # The web SPA compiled into the binary. This is the only place with a
          # JS toolchain: the TypeScript CLI/kap-server bundles are no longer
          # products (the rust engine is), so nothing else runs pnpm here.
          webDist = pkgs.stdenv.mkDerivation {
            pname = "mirri-web-dist";
            inherit version;
            src = jsSrc;
            pnpmWorkspaces = [ "." ] ++ workspaceNames;
            pnpmDeps = pkgs.fetchPnpmDeps {
              pname = "mirri-web-dist";
              inherit version;
              src = jsSrc;
              pnpmWorkspaces = [ "." ] ++ workspaceNames;
              inherit pnpm;
              fetcherVersion = 3;
              # Refresh on every pnpm-lock.yaml / workspace change: the hash
              # covers the fetched dependency tree, so a lockfile edit that is
              # not paired with a new hash fails the build with a mismatch.
              hash = "sha256-ZKppk+RiU9nSkKfsPiQHoSAxHC55QlByGnlJdcPdAlE=";
            };

            nativeBuildInputs = [
              nodejs
              pnpm
              (pkgs.pnpmConfigHook.override { inherit pnpm; })
            ];

            buildPhase = ''
              runHook preBuild
              pnpm --filter=@mirri-ai/mirri-web run build
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp -r apps/mirri-web/dist/. $out/
              runHook postInstall
            '';
          };

          # `mirri` — the rust CLI with the SPA embedded, the same binary the
          # release pipeline ships. The SEA package it replaces packaged the
          # retired TypeScript CLI.
          mirri-code = pkgs.rustPlatform.buildRustPackage {
            pname = "mirri-code";
            inherit version;
            src = rustSrc;

            # Every crate in the workspace is a crates.io registry dependency,
            # so the lockfile carries all the checksums and no vendor hash has
            # to be maintained by hand (`fetchCargoVendor` would).
            cargoDeps = pkgs.rustPlatform.importCargoLock {
              lockFile = ./rust/Cargo.lock;
            };

            cargoBuildFlags = [
              "-p"
              "mirri-cli"
            ];

            # `wrapProgram` is a shell function the makeWrapper setup hook
            # provides — there is no bin/wrapProgram to call, and
            # buildRustPackage merges this into its own toolchain inputs.
            nativeBuildInputs = [ pkgs.makeWrapper ];

            # The workspace suite runs in the `rust-test` CI job; a Nix build
            # only needs to produce the binary, and running the suite here
            # would multiply an already long cold build.
            doCheck = false;

            # rust-embed compiles `crates/mirri-server/web-dist/` into the
            # binary at build time, so the SPA has to be in place before cargo
            # runs. `rust/scripts/stage-web.mjs` does the same thing for local
            # builds.
            preBuild = ''
              rm -rf crates/mirri-server/web-dist
              mkdir -p crates/mirri-server/web-dist
              cp -r ${webDist}/. crates/mirri-server/web-dist/
            '';

            postInstall = ''
              wrapProgram $out/bin/mirri --prefix PATH : ${lib.makeBinPath [ pkgs.ripgrep pkgs.fd ]}
            '';

            meta = {
              description = "Mirri Code";
              homepage = "https://github.com/mirri-ai/mirricode";
              license = lib.licenses.asl20;
              mainProgram = "mirri";
              platforms = systems;
            };
          };
        in
        {
          inherit mirri-code webDist;
          default = mirri-code;
        }
      );

      apps = forAllSystems (pkgs: {
        mirri-code = {
          type = "app";
          program = "${self.packages.${pkgs.system}.mirri-code}/bin/mirri";
        };
        default = self.apps.${pkgs.system}.mirri-code;
      });

      devShells = forAllSystems (pkgs: {
        default =
          let
            nodejs = nodejsFor pkgs;
            pnpm = pnpmFor pkgs;
          in
          pkgs.mkShell {
            packages = [
              nodejs
              pnpm
              pkgs.ripgrep
              pkgs.fd
            ];
          };
      });
    };
}
