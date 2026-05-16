{
  lib,
  rustPlatform,
  pkg-config,
}:
let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = builtins.path {
    path = ../.;
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # `thread_db` is fetched by git rev (see Cargo.toml — pinned to a fork
    # at c36ca728 until aarch64 support is upstreamed). Nix needs the
    # vendored hash explicitly because it can't derive it from Cargo.lock.
    outputHashes = {
      "thread_db-0.1.4" = "sha256-OFKJ9OEo99RAzQHJR4YR3oVGL7Q1uv+ee/9TmkmjqWA=";
    };
  };

  nativeBuildInputs = [ pkg-config ];

  # See https://github.com/NixOS/nixpkgs/blob/nixos-24.05/pkgs/by-name/bu/bugstalker/package.nix#L25-L26
  doCheck = false;

  meta = {
    description = "Rust debugger for Linux x86-64";
    homepage = "https://github.com/godzie44/BugStalker";
    license = lib.licenses.mit;
    mainProgram = "bs";
    platforms = [ "x86_64-linux" ];
  };
}
