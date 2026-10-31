# Writes the .sqlx of each store crate, the data its checked queries are built
# from.
#
# The macros of a crate check each query against one database made from its
# migrations, and write what they learned into its .sqlx while the crate
# compiles - so there is one crate per dialect. The crate is cleaned first, or
# cargo would find nothing to compile and the macros would not run.
#
# Postgres and MariaDB run in the same containers the tests use, from the
# images nix built, and are reached through a port published on the loopback
# interface. The container daemon is the one of whoever runs this script, or
# the one the podman script around it started in a nix build.

const postgres_port = 5432
const mariadb_port = 3306

def main [] {
  let root = (^flake-root | str trim)
  cd $root

  let work = ($root | path join "target" "prepare")
  rm --recursive --force $work
  mkdir $work

  let outcome = (
    try { prepare-all $root $work; null } catch {|error| $error }
  )
  remove-containers
  rm --recursive --force $work

  if $outcome != null {
    error make --unspanned { msg: $outcome.msg }
  }
}

def prepare-all [root: string, work: string] {
  prepare-sqlite $root $work
  prepare-postgres $root
  prepare-mariadb $root
}

def prepare-sqlite [root: string, work: string] {
  let crate = "mcp-semantic-memory-sqlite"
  let database = ($work | path join "sqlite.db")

  for migration in (migrations $root $crate) {
    open --raw $migration | ^sqlite3 $database
  }

  check-crate $root $crate $"sqlite://($database)"
}

def prepare-postgres [root: string] {
  let crate = "mcp-semantic-memory-postgres"
  let port = (
    start-container "postgres" $env.MCP_SEMANTIC_MEMORY_TEST_POSTGRES_ARCHIVE $env.MCP_SEMANTIC_MEMORY_TEST_POSTGRES_IMAGE $postgres_port
  )
  wait-for-postgres "postgres" $port
  ^createdb --host=127.0.0.1 $"--port=($port)" --username=postgres prepare

  for migration in (migrations $root $crate) {
    ^psql --host=127.0.0.1 $"--port=($port)" --username=postgres --dbname=prepare --set=ON_ERROR_STOP=1 --quiet $"--file=($migration)"
  }

  check-crate $root $crate $"postgres://postgres@127.0.0.1:($port)/prepare"
}

def prepare-mariadb [root: string] {
  let crate = "mcp-semantic-memory-mariadb"
  let port = (
    start-container "mariadb" $env.MCP_SEMANTIC_MEMORY_TEST_MARIADB_ARCHIVE $env.MCP_SEMANTIC_MEMORY_TEST_MARIADB_IMAGE $mariadb_port
  )
  wait-for-mariadb "mariadb" $port
  ^mariadb --host=127.0.0.1 $"--port=($port)" --user=root --password=root $"--execute=CREATE DATABASE prepare"

  for migration in (migrations $root $crate) {
    open --raw $migration | ^mariadb --host=127.0.0.1 $"--port=($port)" --user=root --password=root prepare
  }

  check-crate $root $crate $"mysql://root:root@127.0.0.1:($port)/prepare"
}

def start-container [
  name: string
  archive: string
  image: string
  port: int
]: nothing -> int {
  load-image $archive $image
  let container = (container-name $name)
  ^podman --remote rm --force $container o+e> /dev/null
  ^podman --remote run --detach --rm --name $container --publish $"127.0.0.1::($port)" $image o> /dev/null

  ^podman --remote port $container $"($port)/tcp"
  | lines
  | first
  | split row ":"
  | last
  | into int
}

def load-image [archive: string, image: string] {
  let present = (^podman --remote image exists $image | complete).exit_code == 0
  if not $present {
    ^podman --remote load --input $archive o> /dev/null
  }
}

def wait-for-postgres [name: string, port: int] {
  wait-for $name {
    ^pg_isready --host=127.0.0.1 $"--port=($port)" --username=postgres
  }
}

def wait-for-mariadb [name: string, port: int] {
  wait-for $name {
    ^mariadb --host=127.0.0.1 $"--port=($port)" --user=root --password=root $"--execute=SELECT 1"
  }
}

def wait-for [name: string, probe: closure] {
  loop {
    let result = (do $probe | complete)
    if $result.exit_code == 0 {
      break
    }

    let running = (
      ^podman --remote container inspect --format "{{.State.Running}}" (container-name $name)
      | str trim
    )
    if $running != "true" {
      let logs = (^podman --remote logs (container-name $name) | complete)
      error make --unspanned { msg: $"the ($name) container stopped before it was ready:\n($logs.stdout)($logs.stderr)" }
    }
    sleep 200ms
  }
}

def check-crate [root: string, crate: string, url: string] {
  let sqlx = ($root | path join "src" "crates" $crate ".sqlx")
  rm --recursive --force $sqlx
  mkdir $sqlx

  ^cargo clean --package $crate
  with-env { DATABASE_URL: $url, SQLX_OFFLINE: "false", SQLX_OFFLINE_DIR: $sqlx } {
    ^cargo check --package $crate --all-targets
  }
}

def migrations [root: string, crate: string]: nothing -> list<string> {
  glob ($root | path join "src" "crates" $crate "migrations" "*.sql") | sort
}

def container-name [name: string]: nothing -> string {
  $"mcp-semantic-memory-prepare-($name)"
}

def remove-containers [] {
  for name in ["postgres" "mariadb"] {
    do --ignore-errors {
      ^podman --remote rm --force (container-name $name) o+e> /dev/null
    }
  }
}
