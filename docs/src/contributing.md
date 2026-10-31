# Contributing

Enter the dev shell with `nix develop`. It has the `dev` command and everything
it needs, so nothing else has to be installed. See
[Where things are](development.md) for the layout and for what each `dev`
subcommand does.

Before sending a change:

- Run `dev format`, then `dev check`. `dev check` needs Podman or Docker for the
  Postgres and MariaDB tests.
- If the change touches a query or a migration, run `dev prepare` and commit
  what it writes to the `.sqlx/` folders.
- Add tests. A bug fix gets a test that fails without it. A new `Store` method
  gets a check in the shared suite, so all three databases have to pass it.
- If the change makes a page of this book wrong, fix the page in the same
  change. `AGENTS.md` too, if it makes that wrong.

Commits follow [Conventional Commits](https://www.conventionalcommits.org), like
`feat: add the reembed command` or `fix(sqlite): keep the write lock`, because
[releases](releasing.md) are made from them.

## Rules of the code

- Every tunable number comes from the configuration. A constant in the code is
  only for a fixed limit of a dependency, with a comment that says so.
- SQL is written in the function that runs it. Queries use the checked `query!`
  macros, except those on the vector tables, which the build cannot check.
- The library returns typed errors and holds no SQL. The binary uses `anyhow`.
- Lines are at most 80 columns, and indents are two spaces.
- Comments are for code that surprises. A comment that says what the code does
  is a sign the code should be named better.
