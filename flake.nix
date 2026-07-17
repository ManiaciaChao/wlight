{
  description = "Per-monitor DDC and Wayland gamma brightness control";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        rec {
          default = wlight;
          wlight = pkgs.rustPlatform.buildRustPackage {
            pname = "wlight";
            version = "0.1.0";
            src = nixpkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                let
                  name = builtins.baseNameOf path;
                in
                nixpkgs.lib.cleanSourceFilter path type
                && !(
                  type == "directory"
                  && builtins.elem name [
                    ".direnv"
                    "target"
                  ]
                )
                && !builtins.elem name [
                  ".envrc.local"
                  "result"
                ];
            };

            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "--workspace"
              "--bins"
            ];
            cargoTestFlags = [ "--workspace" ];

            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.pkg-config
            ];
            buildInputs = [
              pkgs.libGL
              pkgs.libxkbcommon
              pkgs.udev
              pkgs.wayland
            ];

            postInstall = ''
              install -Dm644 LICENSE "$out/share/licenses/wlight/LICENSE"

              install -Dm644 assets/io.github.wlight.svg \
                "$out/share/icons/hicolor/scalable/apps/io.github.wlight.svg"
              install -Dm644 assets/io.github.wlight.desktop \
                "$out/share/applications/io.github.wlight.desktop"

              install -Dm644 dbus/io.github.wlight.service.in \
                "$out/share/dbus-1/services/io.github.wlight.service"
              substituteInPlace "$out/share/dbus-1/services/io.github.wlight.service" \
                --replace-fail '@out@' "$out"

              install -Dm644 systemd/wlight.service.in \
                "$out/lib/systemd/user/wlight.service"
              substituteInPlace "$out/lib/systemd/user/wlight.service" \
                --replace-fail '@out@' "$out"
            '';

            postFixup = ''
              wrapProgram "$out/bin/wlight-applet" \
                --prefix LD_LIBRARY_PATH : ${
                  nixpkgs.lib.makeLibraryPath [
                    pkgs.libGL
                    pkgs.libxkbcommon
                    pkgs.wayland
                  ]
                }
            '';

            meta = {
              description = "Per-monitor DDC and Wayland gamma brightness control";
              homepage = "https://github.com/ManiaciaChao/wlight";
              license = nixpkgs.lib.licenses.gpl3Only;
              platforms = nixpkgs.lib.platforms.linux;
              mainProgram = "wlight-applet";
            };
          };
        }
      );

      checks = forAllSystems (system: {
        package = self.packages.${system}.wlight;
      });

      apps = forAllSystems (system: {
        default = self.apps.${system}.applet;
        applet = {
          type = "app";
          program = "${self.packages.${system}.wlight}/bin/wlight-applet";
          meta.description = "Launch the wlight graphical applet";
        };
        daemon = {
          type = "app";
          program = "${self.packages.${system}.wlight}/bin/wlightd";
          meta.description = "Run the wlight brightness daemon";
        };
        ctl = {
          type = "app";
          program = "${self.packages.${system}.wlight}/bin/wlightctl";
          meta.description = "Control wlight from the command line";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.wlight ];
            packages = with pkgs; [
              acl
              cargo
              clippy
              dbus
              ddcutil
              nixfmt
              ripgrep
              rust-analyzer
              rustc
              rustfmt
              wayland-utils
            ];
            LD_LIBRARY_PATH = nixpkgs.lib.makeLibraryPath [
              pkgs.libGL
              pkgs.libxkbcommon
              pkgs.udev
              pkgs.wayland
            ];
            RUST_LOG = "wlightd=debug,wlight_backend=debug";
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
      nixosModules.default = import ./nix/nixos-module.nix self;
      homeManagerModules.default = import ./nix/home-manager-module.nix self;
    };
}
