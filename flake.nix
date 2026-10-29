{
  description = "Mirri Code";

  inputs = {
    # Pinned to the 25.11 release channel because nixpkgs-unstable currently
    # ships nodejs_24 = 24.14.1, which trips the >= 24.15.0 floor that
    # `.npmrc`'s `engine-strict=true` enforces during `pnpm install` (the web
    # SPA build is the one place the JS toolchain still runs).
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

      minNodeVersion = "24.15.0";

      # Hardcode to Node.js 24.x; fail the evaluation if the pinned nixpkgs
      # does not offer a new enough 24.x.
      nodejsFor =
        pkgs:
        let
          node = pkgs.nodejs_24;
        in
        if lib.versionAtLeast node.version minNodeVersion then
          node
        else
          throw ''
            Mirri Code requires Node.js >= ${minNodeVersion},
            but nixpkgs only offers ${node.version}.
            Pin a newer nixpkgs revision or update minNodeVersion in flake.nix.
          '';

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
                ./build
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
              hash = "sha256-rdCzRUgKVUNbglshJCMsNuZShe47itqKRRJPinHrnLQ=";
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
