{ pkgs, ... }:

{
  dotenv.enable = true;

  dagger = {
    enable = true;
    version = "v1.0.0-beta.15";

    dang.enable = true;
  };

  packages = with pkgs; [
    lld
    cargo-audit
    cargo-deny
    cargo-dist
    cargo-release
    cargo-watch
  ];

  languages = {
    rust = {
      enable = true;
      channel = "stable";
      targets = [ "wasm32-unknown-unknown" ];
    };
  };
}
