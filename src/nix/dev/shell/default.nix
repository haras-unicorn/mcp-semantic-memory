{ inputs, selfLib, ... }:

{
  self.devShells.default = {
    flake = true;
    value =
      {
        config,
        lib,
        pkgs,
        ...
      }:
      let
        # NOTE: the package set of a dev shell only has some of the haraspkgs
        # overlays, and the functions among them are not flake packages, so
        # they are taken from the composed result of haraspkgs
        inherit (inputs.haraspkgs.lib.${pkgs.stdenv.hostPlatform.system}.overlays.other)
          makePodmanScript
          writeNushellApplication
          ;

        yaml = pkgs.formats.yaml { };

        toml = pkgs.formats.toml { };

        json = pkgs.formats.json { };

        harasPackages = pkgs.harasPackages;

        # NOTE: the toolchain and vendored crates come from the package so the
        # dev shell and the nix build always agree
        mcpMemory = pkgs.${selfLib.overlays.namespace}.mcp-semantic-memory;
        inherit (mcpMemory.passthru) toolchain cargoVendorDir;

        postgresImage = pkgs.${selfLib.overlays.namespace}.test-postgres-image;
        mariadbImage = pkgs.${selfLib.overlays.namespace}.test-mariadb-image;

        # NOTE: the container tests load these images into the container
        # runtime and start their databases from them. The tag comes from the
        # contents, so an image that is already loaded is the right one.
        containerEnvironment = {
          MCP_SEMANTIC_MEMORY_TEST_POSTGRES_ARCHIVE = "${postgresImage}";
          MCP_SEMANTIC_MEMORY_TEST_POSTGRES_IMAGE = "${postgresImage.imageName}:${postgresImage.imageTag}";
          MCP_SEMANTIC_MEMORY_TEST_MARIADB_ARCHIVE = "${mariadbImage}";
          MCP_SEMANTIC_MEMORY_TEST_MARIADB_IMAGE = "${mariadbImage.imageName}:${mariadbImage.imageTag}";
        };

        exportContainerEnvironment = builtins.concatStringsSep "\n" (
          lib.mapAttrsToList (
            name: value: "export ${name}=${lib.escapeShellArg value}"
          ) containerEnvironment
        );

        # NOTE: the TLS is the system's, which is OpenSSL. Cargo is pointed at
        # it here, and the rpath lets the test binaries load it
        opensslLib = "${pkgs.openssl.out}/lib";
        opensslInclude = "${pkgs.openssl.dev}/include";
        opensslRustflags = "-C link-arg=-Wl,-rpath,${opensslLib}";

        # NOTE: the same width and indent as the editorconfig and prettier -
        # passed on the command line, so neither the formatter nor the check
        # needs a rustfmt.toml
        rustfmtConfig = "max_width=80,tab_spaces=2";

        rustfmtOptions = [
          "--edition"
          "2024"
          "--config"
          rustfmtConfig
        ];

        prettierConfig = yaml.generate "prettier.yaml" {
          printWidth = 80;
          tabWidth = 2;
          useTabs = false;
          trailingComma = "all";
          endOfLine = "lf";
          proseWrap = "always";
        };

        cspellConfig = yaml.generate "cspell.yaml" {
          useGitignore = true;
          version = "0.2";
          language = "en";
          ignorePaths = [
            "CHANGELOG.md"
            "CODE_OF_CONDUCT.md"
            "Cargo.lock"
            "flake.lock"
          ];
          words = builtins.fromJSON (builtins.readFile ./words.json);
        };

        markdownlintConfig = json.generate "markdownlint.json" {
          "$schema" =
            "https://raw.githubusercontent.com/DavidAnson/markdownlint/main/schema/markdownlint-config-schema.json";
          # NOTE: prettier pads tables and keeps long code lines and links
          MD013 = {
            tables = false;
            code_blocks = false;
          };
        };

        helixServers = lib.mapAttrs (
          _: server:
          {
            inherit (server) command;
          }
          // lib.optionalAttrs (server.arguments != [ ]) {
            args = server.arguments;
          }
          // lib.optionalAttrs (server.config != { }) {
            inherit (server) config;
          }
        ) config.languages.servers;

        helixLanguages = lib.mapAttrsToList (
          name: language:
          {
            inherit name;
            auto-format = language.auto-format;
          }
          // lib.optionalAttrs (language.servers != [ ]) {
            language-servers = language.servers;
          }
          // lib.optionalAttrs (language.formatter != null) {
            formatter = {
              inherit (language.formatter) command;
            }
            // lib.optionalAttrs (language.formatter.arguments != [ ]) {
              args = language.formatter.arguments;
            };
          }
        ) config.languages.languages;

        helixLanguagesToml = toml.generate "languages.toml" (
          lib.optionalAttrs (helixServers != { }) {
            language-server = helixServers;
          }
          // lib.optionalAttrs (helixLanguages != [ ]) {
            language = helixLanguages;
          }
        );

        nixfmtWrapped = harasPackages.makeBubblewrapWrapper {
          name = "nixfmt";
          package = pkgs.nixfmt;
        };

        prettierWrapped = harasPackages.makeBubblewrapWrapper {
          name = "prettier";
          package = pkgs.prettier;
          files = {
            ".prettierignore" = {
              text = ''
                flake.lock
                CHANGELOG.md
              '';
            };
          };
        };

        taploWrapped = harasPackages.makeBubblewrapWrapper {
          name = "taplo";
          package = pkgs.taplo;
        };

        cspellWrapped = harasPackages.makeBubblewrapWrapper {
          name = "cspell";
          package = pkgs.cspell;
        };

        hxWrapped = harasPackages.makeBubblewrapWrapper {
          name = "hx";
          package = pkgs.helix;
          files = {
            "~/.cache/helix" = {
              mode = "rw";
            };
            ".helix/languages.toml" = {
              mode = "ro";
              source = helixLanguagesToml;
            };
          };
        };

        external = [
          pkgs.nil
          pkgs.nix
          pkgs.fd
          pkgs.yaml-language-server
          pkgs.markdownlint-cli
          pkgs.marksman
          pkgs.mdbook
          pkgs.openssl
        ];
      in
      {
        packages = [
          harasPackages.flake-root
          toolchain
          nixfmtWrapped
          prettierWrapped
          taploWrapped
          cspellWrapped
          hxWrapped
        ]
        ++ external;

        env = containerEnvironment // {
          # NOTE: the tests build a tls client, which needs the system roots
          # and the nix sandbox has none - nix develop drops SSL_CERT_FILE
          # from dev shells, but not SSL_CERT_DIR
          SSL_CERT_DIR = "${pkgs.cacert}/etc/ssl/certs";

          # NOTE: the query macros read their checked queries from .sqlx and
          # not from a database - `dev prepare` is what writes it
          SQLX_OFFLINE = "true";

          OPENSSL_LIB_DIR = opensslLib;
          OPENSSL_INCLUDE_DIR = opensslInclude;
          RUSTFLAGS = opensslRustflags;
        };

        bwrap.files."cspell.yaml".source = cspellConfig;

        # NOTE: points cargo at the crates vendored by nix so it works without
        # network access - ignored by git, and not part of the nix build source
        file.".cargo/config.toml".source = "${cargoVendorDir}/config.toml";

        git = {
          enable = true;
          ignore = {
            templates = [
              "Nix"
              "Rust"
            ];
            patterns = ''
              # Repository

              .env
              docs/book
            '';
          };
          attributes = ''
            * text=auto eol=lf
          '';
        };

        editorconfig = {
          enable = true;
          settings."*" = {
            end_of_line = "lf";
            charset = "utf-8";
            max_line_length = 80;
            indent_style = "space";
            indent_size = 2;
            tab_width = 2;
            insert_final_newline = true;
            trim_trailing_whitespace = true;
          };
        };

        languages = {
          servers = {
            nil.command = "nil";

            # NOTE: rust-analyzer formats with rustfmt, so this is how the
            # editor gets the width and indent - it passes the edition itself
            rust-analyzer = {
              command = "rust-analyzer";
              config.rustfmt.extraArgs = [
                "--config"
                rustfmtConfig
              ];
            };

            taplo = {
              command = "taplo";
              arguments = [
                "lsp"
                "stdio"
              ];
            };

            yaml-language-server = {
              command = "yaml-language-server";
              arguments = [ "--stdio" ];
            };

            marksman = {
              command = "marksman";
              arguments = [ "server" ];
            };
          };

          languages = {
            nix = {
              servers = [ "nil" ];
              formatter = {
                command = lib.getExe pkgs.nixfmt;
                arguments = [
                  "--width"
                  "80"
                  "--filename"
                  "%{buffer_name}"
                ];
              };
            };

            rust.servers = [ "rust-analyzer" ];

            toml.servers = [ "taplo" ];

            yaml = {
              servers = [ "yaml-language-server" ];
              formatter = {
                command = lib.getExe pkgs.prettier;
                arguments = [
                  "--config"
                  (builtins.toString prettierConfig)
                  "--parser"
                  "yaml"
                ];
              };
            };

            markdown = {
              servers = [ "marksman" ];
              formatter = {
                command = lib.getExe pkgs.prettier;
                arguments = [
                  "--config"
                  (builtins.toString prettierConfig)
                  "--parser"
                  "markdown"
                ];
              };
            };
          };
        };

        license = {
          enable = true;
          spdx = "MIT";
          path = "LICENSE.md";
          attrs = {
            "<year>" = "2026";
            "<copyright holders>" = "github@haras-unicorn.net";
          };
        };

        codeOfConduct.contributorCovenant = {
          enable = true;
          path = "CODE_OF_CONDUCT.md";
        };

        cli.commands = {
          docs = {
            description = "Build the documentation.";
            packages = [ pkgs.nix ];
            commands = ''nix build ".#docs" --no-link --print-out-paths'';
          };

          prepare = {
            description = "Write the .sqlx of each store crate, the data its checked queries are built from. Run it after changing a query or a migration.";
            commands = lib.getExe (makePodmanScript {
              name = "prepare";
              images = [
                postgresImage
                mariadbImage
              ];
              # NOTE: the databases run in the test images, so this script only needs
              # their clients - the podman script around it brings the daemon in a
              # nix build, and outside of one the daemon is the caller's
              text = lib.getExe (writeNushellApplication {
                name = "prepare-databases";
                runtimeInputs = [
                  harasPackages.flake-root
                  toolchain
                  pkgs.podman
                  pkgs.sqlite
                  pkgs.postgresql
                  pkgs.mariadb_118
                ];
                environment = containerEnvironment;
                text = builtins.readFile ./prepare.nu;
              });
            });
          };
        };

        workflows = {
          check = {
            on.pull_request = { };

            matrix.os = [ "ubuntu-latest" ];

            installable = null;

            steps = [
              {
                run = "nix flake check --show-trace --print-build-logs --max-jobs 1 --keep-going";
              }
            ];
          };

          # NOTE: the book is built by nix and deployed to GitHub Pages. The
          # job needs the github-pages environment for the deploy action, and
          # the repository's Pages source set to GitHub Actions. The artifact
          # is a copy because the action does not follow the result link.
          docs = {
            on = {
              push.branches = [ "main" ];
              workflow_dispatch = { };
            };

            permissions = {
              pages = "write";
              "id-token" = "write";
            };

            installable = ".#docs";

            steps = [
              { run = "cp -rL result site"; }
              { uses = "actions/configure-pages@v5"; }
              {
                uses = "actions/upload-pages-artifact@v3";
                "with".path = "site";
              }
              {
                id = "deployment";
                uses = "actions/deploy-pages@v4";
              }
            ];

            jobs.build = {
              environment = {
                name = "github-pages";
                url = "\${{ steps.deployment.outputs.page_url }}";
              };
              concurrency = {
                group = "pages";
                cancel-in-progress = false;
              };
            };
          };

          # NOTE: needs a RELEASE_PLZ_TOKEN personal access token (and a
          # CARGO_REGISTRY_TOKEN secret). Tags and releases created with the
          # default GITHUB_TOKEN do not trigger the build workflow.
          #
          # The release-plz.toml makes one tag for the whole repository, from
          # the -cli package, and the build workflow puts the binaries on it.
          # The package list there must name every published crate.
          release = {
            on.push.branches = [ "main" ];

            permissions = {
              contents = "write";
              "pull-requests" = "write";
            };

            installable = null;
            installNix = false;
            cachix = null;

            steps = [
              { uses = "dtolnay/rust-toolchain@stable"; }
              {
                uses = "release-plz/action@v0.5";
                "with".command = "release";
                env = {
                  GITHUB_TOKEN = "\${{ secrets.RELEASE_PLZ_TOKEN }}";
                  CARGO_REGISTRY_TOKEN = "\${{ secrets.CARGO_REGISTRY_TOKEN }}";
                };
              }
              {
                uses = "release-plz/action@v0.5";
                "with".command = "release-pr";
                env = {
                  GITHUB_TOKEN = "\${{ secrets.RELEASE_PLZ_TOKEN }}";
                };
              }
            ];
          };

          build = {
            on.release.types = [ "published" ];

            permissions.contents = "write";

            matrix.os = [
              "ubuntu-latest"
              "ubuntu-24.04-arm"
            ];

            installable = null;

            steps = [
              {
                run = ''
                  path="$(nix build --no-link --print-out-paths --show-trace .#mcp-semantic-memory)"
                  system="$(nix eval --impure --raw --expr builtins.currentSystem)"
                  cp "$path/bin/mcp-semantic-memory" "mcp-semantic-memory-$system"
                  gh release upload "$TAG" "mcp-semantic-memory-$system" --clobber
                '';
                env = {
                  GH_TOKEN = "\${{ secrets.GITHUB_TOKEN }}";
                  TAG = "\${{ github.event.release.tag_name }}";
                };
              }
            ];
          };
        };

        check.checks = {
          clippy = {
            packages = [ toolchain ];
            commands = ''
              CARGO_HOME="$(mktemp -d)"
              export CARGO_HOME
              trap 'rm -rf "$CARGO_HOME"' EXIT
              cp ${cargoVendorDir}/config.toml "$CARGO_HOME/config.toml"
              export SQLX_OFFLINE=true
              export OPENSSL_LIB_DIR=${opensslLib}
              export OPENSSL_INCLUDE_DIR=${opensslInclude}
              export RUSTFLAGS="${opensslRustflags}"
              cargo clippy --workspace --all-targets -- -D warnings
            '';
          };

          test = {
            podman = {
              enable = true;
              images = [
                postgresImage
                mariadbImage
              ];
            };
            packages = [
              toolchain
              pkgs.stdenv.cc
            ];
            commands = ''
              CARGO_HOME="$(mktemp -d)"
              export CARGO_HOME
              trap 'rm -rf "$CARGO_HOME"' EXIT
              cp ${cargoVendorDir}/config.toml "$CARGO_HOME/config.toml"
              export SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs
              export SQLX_OFFLINE=true
              export OPENSSL_LIB_DIR=${opensslLib}
              export OPENSSL_INCLUDE_DIR=${opensslInclude}
              export RUSTFLAGS="${opensslRustflags}"
              ${exportContainerEnvironment}
              cargo test --workspace
            '';
          };

          cspell = {
            packages = [ cspellWrapped ];
            commands = "cspell lint . --no-progress";
          };

          markdownlint = {
            packages = [ pkgs.markdownlint-cli ];
            commands = ''
              markdownlint --config ${markdownlintConfig} --ignore-path .gitignore --ignore CHANGELOG.md --ignore CODE_OF_CONDUCT.md --ignore LICENSE.md --ignore docs/src/SUMMARY.md .
            '';
          };

          nil = {
            packages = [
              pkgs.fd
              pkgs.nil
            ];
            commands = "fd --glob '*.nix' . -0 | xargs -0 -r nil diagnostics";
          };
        };

        format.formatters = {
          nixfmt = {
            packages = [ nixfmtWrapped ];
            command = "nixfmt";
            options = [
              "--width"
              "80"
            ];
            includes = [ "*.nix" ];
            excludes = [ "target/**" ];
          };

          taplo = {
            packages = [ taploWrapped ];
            command = "taplo";
            options = [ "format" ];
            includes = [ "*.toml" ];
            excludes = [ "target/**" ];
          };

          prettier = {
            packages = [ prettierWrapped ];
            command = "prettier";
            options = [
              "--write"
              "--config"
              (builtins.toString prettierConfig)
            ];
            includes = [
              "*.json"
              "*.md"
              "*.yaml"
              "*.yml"
            ];
            # NOTE: .sqlx is generated by `dev prepare`, which would be
            # undone by the formatter and the other way around
            excludes = [
              "target/**"
              "**/.sqlx/**"
              "flake.lock"
              "CHANGELOG.md"
              "CODE_OF_CONDUCT.md"
              "LICENSE.md"
            ];
          };

          rustfmt = {
            packages = [ toolchain ];
            command = "rustfmt";
            options = rustfmtOptions;
            includes = [ "*.rs" ];
            excludes = [ "target/**" ];
          };
        };
      };
  };
}
