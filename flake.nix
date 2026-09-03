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

          nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
          buildInputs = [ pkgs.libusb1 pkgs.alsa-lib pkgs.pipewire ];

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
            rustPlatform.bindgenHook
          ];
          buildInputs = with pkgs; [ libusb1 alsa-lib pipewire ];

          # Handy while developing against real hardware: sox measures what came
          # back out of it, and wireplumber's wpctl says whether the driver's
          # nodes turned up in the graph. `pipewire` itself is a build input,
          # which is also where pw-link and pw-play come from.
          packages = with pkgs; [ alsa-utils usbutils sox wireplumber ];
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
                installed, so `systemctl --user start ns6` works on demand.
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
                  , TAG+="systemd", ENV{SYSTEMD_USER_WANTS}+="ns6.service"''
              }
            '';

            # A user service, not a system one. The driver publishes its own
            # PipeWire sink and source, and PipeWire is a session service: its
            # socket lives in the user's runtime directory, which a root service
            # has no business reaching into and no way to pick between if two
            # people are logged in. The ALSA MIDI port belongs to the same
            # session, so both halves land where the applications are.
            systemd.user.services.ns6 = {
              description = "Numark NS6 userspace driver";
              serviceConfig = {
                ExecStart = lib.getExe cfg.package;
                # open() waits a little for the device and then gives up, so a
                # few retries cover a slow enumeration without spinning forever
                # while the controller is switched off.
                Restart = "on-failure";
                RestartSec = 3;
                # The sandboxing a system unit would carry - ProtectSystem,
                # PrivateTmp - needs privileges a user manager does not reliably
                # have, and buys little for a process that already has exactly
                # its own user's reach. What it does need is raw usbfs, the ALSA
                # sequencer and the session's PipeWire socket.
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
