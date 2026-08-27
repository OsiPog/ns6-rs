{
  description = "ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.system}.ns6;

        ns6 = pkgs.rustPlatform.buildRustPackage {
          pname = "ns6";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.libusb1 pkgs.alsa-lib ];

          meta = with pkgs.lib; {
            description = "Userspace MIDI driver for the Numark NS6 DJ controller";
            longDescription = ''
              The Numark NS6 exposes only vendor-specific USB interfaces, so no kernel
              driver binds it and no ALSA MIDI port appears. ns6 speaks the device's
              Ploytec protocol over libusb and publishes an ALSA sequencer MIDI port
              that Mixxx and other MIDI software can use.
            '';
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "ns6";
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            pkg-config
          ];
          buildInputs = with pkgs; [ libusb1 alsa-lib ];

          # Handy while developing against real hardware.
          packages = with pkgs; [ alsa-utils usbutils ];
        };
      });

      # NixOS module fragment: grants the logged-in user access to the NS6's
      # usbfs node, which the driver needs since it talks to the raw device.
      nixosModules.default = { ... }: {
        services.udev.extraRules = builtins.readFile ./udev/70-numark-ns6.rules;
      };
    };
}
