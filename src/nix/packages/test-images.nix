{
  # NOTE: the container tests start their databases from these, so the only
  # images they ever run are ones nix built - nothing is pulled from a
  # registry. They are for tests only: no password, open to the network, and
  # a TLS certificate made when the container starts and signed by nobody, so
  # a client can tell encrypted from not, and trusted from not.
  self.overlays.overlays.test-images = {
    flake = true;
    value =
      final: _prev:
      let
        # NOTE: pgvector 0.8 or newer, for the iterative index scans
        postgres = final.postgresql.withPackages (extensions: [
          extensions.pgvector
        ]);

        # NOTE: 11.7 or newer has the vector type and index
        mariadb = final.mariadb_118;

        # NOTE: an empty config file, because openssl would otherwise look for
        # one in /etc/ssl, where the image has none
        certificate = directory: ''
          openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
            -config /dev/null -subj /CN=localhost \
            -keyout ${directory}/server.key -out ${directory}/server.crt
          chmod 600 ${directory}/server.key
        '';

        postgresEntrypoint = final.writeShellApplication {
          name = "entrypoint";
          runtimeInputs = [
            postgres
            final.openssl
            final.coreutils
          ];
          text = ''
            initdb --username=postgres --auth=trust --encoding=UTF8 \
              --no-locale --pgdata=/data/postgres
            echo 'host all all all trust' >> /data/postgres/pg_hba.conf
            ${certificate "/data"}
            exec postgres -D /data/postgres -c listen_addresses='*' \
              -c unix_socket_directories=/tmp -c ssl=on \
              -c ssl_cert_file=/data/server.crt \
              -c ssl_key_file=/data/server.key
          '';
        };

        mariadbEntrypoint = final.writeShellApplication {
          name = "entrypoint";
          runtimeInputs = [
            mariadb
            final.openssl
            final.coreutils
            final.gnugrep
            final.gnused
            final.gawk
          ];
          text = ''
            mariadb-install-db --datadir=/data/mariadb \
              --auth-root-authentication-method=normal --skip-test-db
            cat > /tmp/init.sql <<'SQL'
            CREATE USER IF NOT EXISTS 'root'@'%' IDENTIFIED BY 'root';
            GRANT ALL PRIVILEGES ON *.* TO 'root'@'%' WITH GRANT OPTION;
            SQL
            ${certificate "/tmp"}
            exec mariadbd --datadir=/data/mariadb --port=3306 \
              --bind-address=0.0.0.0 --skip-name-resolve \
              --socket=/tmp/mariadb.sock --pid-file=/tmp/mariadb.pid \
              --innodb-use-native-aio=0 --init-file=/tmp/init.sql \
              --ssl-cert=/tmp/server.crt --ssl-key=/tmp/server.key
          '';
        };

        # NOTE: both servers refuse to run as root, so the images run as
        # nobody, and the directories they write to are open to it. initdb
        # and the MariaDB scripts start programs through /bin/sh.
        mkImage =
          {
            name,
            entrypoint,
            port,
          }:
          final.dockerTools.buildLayeredImage {
            inherit name;
            contents = [
              final.dockerTools.fakeNss
              final.dockerTools.binSh
              entrypoint
            ];
            extraCommands = ''
              mkdir -m 1777 tmp
              mkdir -m 777 data
            '';
            config = {
              Entrypoint = [ "${entrypoint}/bin/entrypoint" ];
              User = "nobody";
              Env = [ "HOME=/tmp" ];
              ExposedPorts."${builtins.toString port}/tcp" = { };
            };
          };
      in
      {
        test-postgres-image = mkImage {
          name = "mcp-semantic-memory-test-postgres";
          entrypoint = postgresEntrypoint;
          port = 5432;
        };

        test-mariadb-image = mkImage {
          name = "mcp-semantic-memory-test-mariadb";
          entrypoint = mariadbEntrypoint;
          port = 3306;
        };
      };
  };
}
