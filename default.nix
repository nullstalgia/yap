{
  lib,
  rustPlatform,
  pkg-config,
  udev,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "yap";
  version = "0.1.1-pre.0";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    outputHashes = {
      "ansi-to-tui-7.0.0" = "sha256-SkFK8UAwGITI4csQeb9GWXPmFMhaKv/Z3Y8e7eR403Y=";
      "copy_to_output-2.2.1" = "sha256-b2yY9EiHqfZ8DrD6V1BG1MqLU8fV4Wook+ysUTNzn/k=";
      "defmt-decoder-1.0.0" = "sha256-7ddgJJpduLtFwAKFBfO3+kRI1WcPh5sMtcdqgqJObCA=";
    };
  };

  nativeBuildInputs = [
    pkg-config
    openssl
  ];

  buildInputs = [
    udev
    openssl
  ];

  meta = with lib; {
    description = "A friendly serial terminal application.";
    homepage = "https://github.com/nullstalgia/yap";
    license = licenses.mit;
    maintainers = [];
  };
}
