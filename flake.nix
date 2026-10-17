{
  description = "Godon robots - the tender body engine (rung 10 rust port)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.pkgsBuildHost.rust-bin.stable.latest.default;

        # The workspace builds whole; the engine binary is the one
        # deliverable (strains compile in - the purpose-profile ruling).
        godon-engine = { version ? (if builtins.getEnv "GODON_VERSION" == "" then
          "DEV_BUILD"
        else
          builtins.getEnv "GODON_VERSION") }:
          pkgs.stdenv.mkDerivation {
            pname = "godon-robots";
            inherit version;
            src = ./.;

            # Pure rust deps (postgres wire, reqwest without TLS, serde,
            # chrono, sha2, base64) - no openssl, no pkg-config. The cert
            # env serves cargo's own HTTPS dep fetch during the build.
            nativeBuildInputs = with pkgs; [ rustToolchain ];

            env = {
              SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
              NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
              CURL_CA_BUNDLE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            };

            buildPhase = ''
              echo "Building godon-engine version: ${version}"
              export CARGO_HOME=$PWD/.cargo-home
              cargo build --release
            '';

            installPhase = ''
              mkdir -p $out/bin
              cp target/release/godon-engine $out/bin/godon-engine
              chmod +x $out/bin/godon-engine
            '';

            meta = with pkgs.lib; {
              description = "The godon tender body engine - one binary, the component dispatch at argv[1]";
              license = licenses.agpl3Only;
              platforms = platforms.linux;
            };
          };

      in {
        packages.default = godon-engine { };
        packages.godon-engine = godon-engine;

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [ rustToolchain rust-analyzer cargo-watch ];

          shellHook = ''
            echo "Godon robots development environment"
            echo "Rust: $(rustc --version)"
          '';
        };
      });
}
