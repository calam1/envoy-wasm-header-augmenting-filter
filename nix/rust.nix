{ sources ? import ./sources.nix }:

let
  pkgs =
    import sources.nixpkgs { overlays = [ (import sources.nixpkgs-mozilla) ]; };
  channel = "nightly";
  date = "2020-08-06";
  targets = [ "wasm32-unknown-unknown" ];
  chan = pkgs.rustChannelOfTargets channel date targets;
in chan
