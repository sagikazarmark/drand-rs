{ pkgs, ... }:

{
  dotenv.enable = true;

  packages = with pkgs; [
    just

    lld
    cargo-audit
    cargo-deny
    cargo-dist
    cargo-hack
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
