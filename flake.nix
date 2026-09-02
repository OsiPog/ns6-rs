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
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.ns6;

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

          # Handy while developing against real hardware. sox measures what came
          # back out of it; pulseaudio is here for pactl, which is what loads the
          # PipeWire pipe modules the audio side is driven through.
          packages = with pkgs; [ alsa-utils usbutils sox pulseaudio ];
        };
      });

      # NixOS module. Installs the driver, grants access to the NS6's usbfs node
      # and runs the bridge as a service that udev starts when the controller
      # appears on the bus - which, since the NS6 has its own power switch, is
      # not the same moment as plugging the cable in.
      #
      #   imports = [ inputs.ns6.nixosModules.default ];
      #   services.ns6.enable = true;
      nixosModules.default = { config, lib, pkgs, ... }:
        let cfg = config.services.ns6;
        in {
          options.services.ns6 = {
            enable = lib.mkEnableOption "the Numark NS6 userspace MIDI driver";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.ns6;
              defaultText = lib.literalExpression "ns6.packages.\${system}.ns6";
              description = "The ns6 package to use.";
            };

            autoStart = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = ''
                Start the bridge automatically when the controller appears on the
                USB bus. With this off the udev rule and the unit are still
                installed, so `systemctl start ns6` works on demand.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];

            # Without `uaccess` the usbfs node reverts to root ownership on every
            # replug, since nothing in the kernel claims the device. Granting it
            # to the seat also means the driver can be run by hand.
            services.udev.extraRules = ''
              SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTRS{idVendor}=="15e4", ATTRS{idProduct}=="0079", TAG+="uaccess", GROUP="wheel", MODE="0660"${
                lib.optionalString cfg.autoStart ''
                  , TAG+="systemd", ENV{SYSTEMD_WANTS}+="ns6.service"''
              }
            '';

            systemd.services.ns6 = {
              description = "Numark NS6 userspace MIDI driver";
              serviceConfig = {
                ExecStart = lib.getExe cfg.package;
                # open() waits a little for the device and then gives up, so a
                # few retries cover a slow enumeration without spinning forever
                # while the controller is switched off.
                Restart = "on-failure";
                RestartSec = 3;
                # Needs raw usbfs and the ALSA sequencer, and nothing else.
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                NoNewPrivileges = true;
                RestrictAddressFamilies = [ "AF_UNIX" "AF_NETLINK" ];
              };
              startLimitIntervalSec = 60;
              startLimitBurst = 5;
            };
          };
        };
    };
}
