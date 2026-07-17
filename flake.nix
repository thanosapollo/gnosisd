{
  description = "Local daemon core for Gnosis knowledge and learning clients";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "gnosis-core";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            postInstall = ''
              install -Dm644 deployment/gnosisd.service \
                $out/share/systemd/user/gnosisd.service
              install -Dm644 deployment/gnosis-backup.service \
                $out/share/systemd/user/gnosis-backup.service
              install -Dm644 deployment/gnosis-backup.timer \
                $out/share/systemd/user/gnosis-backup.timer
              install -Dm644 deployment/server.env.example \
                $out/share/doc/gnosis-core/server.env.example
              install -Dm644 docs/raspberry-pi.org \
                $out/share/doc/gnosis-core/raspberry-pi.org
            '';

            meta = {
              description = "Local daemon core for Gnosis knowledge and learning clients";
              license = pkgs.lib.licenses.gpl3Plus;
              platforms = systems;
            };
          };
        }
      );

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              cargo
              clippy
              rust-analyzer
              rustc
              rustfmt
            ];
          };
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.nixfmt
      );
    };
}
