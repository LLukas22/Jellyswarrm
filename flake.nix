{
  description = "Jellyswarrm - Bring all your Jellyfin servers together";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Import jellyfin-web from nixpkgs instead of building ui
        jellyfinWeb = pkgs.jellyfin-web;

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
        };

        jellyswarrm = pkgs.rustPlatform.buildRustPackage {
          pname = "jellyswarrm";
          version = "0.2.1";

          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeBinaryWrapper
          ];

          buildInputs = with pkgs; [
            sqlite
          ];

          env = {
            JELLYSWARRM_SKIP_UI = "1";
          };

          preBuild = ''
            mkdir -p crates/jellyswarrm-proxy/static
            cp -r ${jellyfinWeb}/share/jellyfin-web/* crates/jellyswarrm-proxy/static/
            cat > crates/jellyswarrm-proxy/static/ui-version.env <<EOF
            UI_VERSION=${jellyfinWeb.version}
            UI_COMMIT=nix
            EOF
          '';

          postInstall = ''
            wrapProgram $out/bin/jellyswarrm-proxy \
              --set SSL_CERT_FILE ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
          '';
        };

      in
      {
        packages = {
          default = jellyswarrm;
          jellyswarrm = jellyswarrm;
        };

      # NixOS module for the service
      nixosModules.default = { config, lib, pkgs, ... }:
        with lib;
        let
          cfg = config.services.jellyswarrm;
        in
        {
          options.services.jellyswarrm = {
            enable = mkEnableOption "Jellyswarrm reverse proxy for Jellyfin servers";

            package = mkOption {
              type = types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.jellyswarrm;
              defaultText = literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.jellyswarrm";
              description = "The Jellyswarrm package to use";
            };

            user = mkOption {
              type = types.str;
              default = "jellyswarrm";
              description = "User account under which Jellyswarrm runs";
            };

            group = mkOption {
              type = types.str;
              default = "jellyswarrm";
              description = "Group under which Jellyswarrm runs";
            };

            dataDir = mkOption {
              type = types.path;
              default = "/var/lib/jellyswarrm";
              description = "Directory where Jellyswarrm stores its data";
            };

            host = mkOption {
              type = types.str;
              default = "0.0.0.0";
              description = "Host address to bind to";
            };

            port = mkOption {
              type = types.port;
              default = 3000;
              description = "Port to listen on";
            };

            username = mkOption {
              type = types.str;
              default = "admin";
              description = "Admin username for Jellyswarrm UI";
            };

            passwordFile = mkOption {
              type = types.nullOr types.path;
              default = null;
              description = ''
                Path to a file containing the admin password.
                If not set, defaults to "jellyswarrm" (insecure).
              '';
            };

            openFirewall = mkOption {
              type = types.bool;
              default = false;
              description = "Open the firewall for the Jellyswarrm port";
            };

            extraEnvironment = mkOption {
              type = types.attrsOf types.str;
              default = {};
              description = "Extra environment variables to pass to Jellyswarrm";
              example = {
                RUST_LOG = "info";
              };
            };
          };

          config = mkIf cfg.enable {
            users.users.${cfg.user} = {
              isSystemUser = true;
              group = cfg.group;
              home = cfg.dataDir;
              createHome = true;
              description = "Jellyswarrm service user";
            };

            users.groups.${cfg.group} = {};

            systemd.services.jellyswarrm = {
              description = "Jellyswarrm Jellyfin reverse proxy";
              after = [ "network.target" ];
              wantedBy = [ "multi-user.target" ];

              serviceConfig = {
                Type = "simple";
                User = cfg.user;
                Group = cfg.group;
                ExecStart = "${cfg.package}/bin/jellyswarrm-proxy";
                WorkingDirectory = cfg.dataDir;
                Restart = "on-failure";
                RestartSec = "5s";

                # Security hardening
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                ReadWritePaths = [ cfg.dataDir ];
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
                RestrictNamespaces = true;
                LockPersonality = true;
                RestrictRealtime = true;
                RestrictSUIDSGID = true;
                RemoveIPC = true;
                PrivateMounts = true;
              } // (
                if cfg.passwordFile != null
                then {
                  LoadCredential = "password:${cfg.passwordFile}";
                  ExecStart = "${pkgs.bash}/bin/bash -c 'JELLYSWARRM_PASSWORD=$(cat $CREDENTIALS_DIRECTORY/password) exec ${cfg.package}/bin/jellyswarrm-proxy'";
                }
                else {
                  ExecStart = "${cfg.package}/bin/jellyswarrm-proxy";
                }
              );

              environment = {
                JELLYSWARRM_HOST = cfg.host;
                JELLYSWARRM_PORT = toString cfg.port;
                JELLYSWARRM_USERNAME = cfg.username;
                JELLYSWARRM_DATA_DIR = cfg.dataDir;
              } // cfg.extraEnvironment // (
                if cfg.passwordFile == null
                then { JELLYSWARRM_PASSWORD = "jellyswarrm"; }
                else {}
              );
            };

            networking.firewall = mkIf cfg.openFirewall {
              allowedTCPPorts = [ cfg.port ];
            };
          };
        };
    };
}
