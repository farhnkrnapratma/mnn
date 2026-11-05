## Git workflow

This repo uses **git-flow-next** (classic preset — https://git-flow.sh/). The config is committed in `.gitflow`; on a fresh clone run `git-flow init --preset=classic --defaults --main main --shared`, then `bash .githooks/install.sh` to install the git hooks (`bun install` does both the hooks and the commitlint tooling through the `prepare` script).

- `main` — release only. Never commit directly; only `release/*` and `hotfix/*` branches merge into it (and get tagged).
- `develop` — integration branch. `feature/*` and `bugfix/*` branch from and merge back into it.
- `feature/<name>`, `bugfix/<name>` — parent `develop`. `git-flow feature start <name>` / `git-flow feature finish <name>`.
- `release/<name>`, `hotfix/<name>` — parent `main`, started from `develop`/`main` respectively, merge into `main`, tagged on finish.

**Always sign off commits** (`git commit -s ...`, including on `git-flow` `feature`/`release`/`hotfix` finish operations).

Commit messages must follow [Conventional Commits](https://www.conventionalcommits.org), enforced by commitlint (`commitlint.config.js`) in the `commit-msg` hook and in CI; validate a candidate message with `printf '%s' "<message>" | bunx commitlint`.

CI runs on CircleCI (`.circleci/config.yml`): `reuse lint`, format/lint, tests, `cargo deny`, secret scan (trufflehog) and commitlint on every push.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- When graphify-out/graph.json exists, run `graphify query "<question>"` before your first grep, search or source-file read for a codebase question, and put this rule in every subagent prompt that explores code. Read raw files only for lines the graph or an earlier read already located. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
