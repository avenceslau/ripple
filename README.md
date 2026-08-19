# monoripple

monoripple finds JavaScript and TypeScript applications affected by a change at symbol precision. It uses Oxc for parsing and semantic binding analysis instead of treating every declared package dependency as a runtime dependency.

## Features

- JavaScript, JSX, TypeScript, and TSX
- npm-compatible workspace package manifests and package exports
- named, default, namespace, and re-exported bindings
- static namespace-member narrowing
- literal dynamic imports and CommonJS `require()`
- imported non-JavaScript assets
- separate runtime and type dependency graphs
- base/current graph union for deletions, renames, and removed exports
- deploy and typecheck affected queries
- Cargo workspace path-dependency propagation for Rust tasks
- runtime module and workspace package cycle detection
- structured diagnostics with warning policies
- local content-addressed parse cache
- external target-discovery plugins
- Vite source roots and Cargo-backed generated Worker targets
- multiple impact paths with per-edge source explanations

## Install

```sh
cargo install monoripple
```

## Usage

```sh
monoripple --root /path/to/monorepo affected --base origin/main
monoripple --root /path/to/monorepo affected --base origin/main --format json
monoripple --root /path/to/monorepo affected --base origin/main --task typecheck
monoripple --root /path/to/monorepo why @acme/app --base origin/main
monoripple --root /path/to/monorepo why @acme/app --base origin/main --ui
monoripple --root /path/to/monorepo check
```

By default monoripple reports packages with a `deploy` script. Use `--target <script>` to select another package script, or `--target all` to include every package with an inferred entrypoint.

## Task runners

Run a Turbo or Vite+ task only for affected packages:

```sh
monoripple run deploy --base origin/main
monoripple run deploy --base origin/main --runner vite-plus
monoripple run deploy --base origin/main -- --environment staging
monoripple run check:type --base origin/main
monoripple run test --base origin/main
```

Test targets include co-located `*.test.*` and `*.spec.*` files, files under `test`, `tests`, and `__tests__`, and Vitest or Jest configuration files. For Workers projects, test reachability also follows Wrangler service and Durable Object script references to workspace workers whose builds participate in the test environment.

`run` follows local `turbo.json` task dependencies when choosing filterable packages. Typecheck, lint, format, and aggregate check tasks use every source file in the selected package rather than requiring an application entrypoint. Other scripts use local JavaScript and TypeScript paths from their command as additional roots. `check:type` selects typecheck mode automatically; `--mode` remains available as an override.

`auto` selects Vite+ when the root package declares `vite-plus` or contains `node_modules/.bin/vp`; otherwise it uses Turbo through the detected package manager. Use `--runner` to override this with `vite-plus`, `pnpm`, `npx`, `bun`, `yarn`, or a direct `turbo` binary. Use `--print` to inspect the command without executing it, and repeat `--runner-arg` for task-runner options:

```sh
monoripple run deploy --base origin/main --print --runner-arg=--dry-run=json
monoripple run deploy --base origin/main --runner vite-plus --runner-arg=--cache
```

The wrapper exits successfully without invoking a runner when no packages are affected. For shell composition, both output formats emit repeated pnpm-style filter arguments:

```sh
pnpm turbo run deploy $(monoripple affected --base origin/main --format turbo)
vp run $(monoripple affected --base origin/main --format vite-plus) deploy
```

When the affected set is empty, the composed form emits an impossible package filter instead of running every package. Turbo fails safely; Vite+ warns and exits successfully without running a task.

`graph` serves an interactive dependency-graph visualization over `http://127.0.0.1` and opens it in your browser:

```sh
monoripple --root /path/to/monorepo graph --base origin/main
monoripple --root /path/to/monorepo graph --base origin/main --scope all
monoripple --root /path/to/monorepo graph --base origin/main --port 7654 --no-open
```

Use `--port` to pin a port (default picks a free one), `--no-open` to skip launching the browser, and `--output file.html` to also save the page. The default `affected` scope shows the symbol-level subgraph from changed declarations to deployment targets; `all` shows the whole repository at module granularity. The graph is arranged left to right as an impact flow: changed declaration → consumer → deployment target. Arrowheads show the impact direction, with solid runtime relationships and dashed blue type-only relationships.

The first affected target is selected automatically. Its numbered reason path stays labeled while unrelated nodes and edges are dimmed. When several routes connect a change to the target, path buttons switch between up to 20 alternatives. Each hop explains the exact import, symbol reference, module load, or target relationship and includes its source line and column when available.

Selecting a path step shows the node’s package, file, symbol, dependencies, and consumers; the affected-target list provides shortcuts to every deployment reason. Manifest changes on directly affected packages list their exact changed inputs. When targets add the same workspace dependency, the view adds shared export nodes and shows exactly which runtime and type exports each target uses.

`why --ui` opens an interactive impact-path viewer using the same changed → consumer → target direction and edge explanations as the graph. Use up/down or `j`/`k` to navigate steps, left/right or `h`/`l` to switch paths, and `q` or Escape to exit. Directly affected packages list the exact changed build inputs in both text and UI modes.

Warnings can be hidden or promoted to errors:

```sh
monoripple check --warnings off
monoripple check --warnings error
```

The local cache defaults to `$XDG_CACHE_HOME/monoripple` or `~/.cache/monoripple`. Disable it with `--no-cache`, inspect it with `--cache-report`, or override it with `MONORIPPLE_CACHE_DIR`.

## Plugins

A repository may configure explicit external plugins in `monoripple.json`:

```json
{
  "plugins": [
    {
      "name": "vite",
      "command": ["pnpm", "exec", "monoripple-vite"]
    }
  ],
  "diagnostics": {
    "exclude": [
      {
        "code": "MONORIPPLE_NAMESPACE_DYNAMIC_ACCESS",
        "path": "**/*.generated.ts",
        "reason": "generated by the framework"
      }
    ]
  }
}
```

monoripple sends a versioned JSON request on stdin. Plugins return target roots and optional diagnostics:

```json
{
  "targets": [
    {
      "package": "web-app",
      "roots": ["apps/web/src/router.tsx"]
    }
  ],
  "edges": [
    {
      "consumer": { "path": "apps/web/src/client.ts", "symbol": "create" },
      "dependency": { "path": "apps/api/src/index.ts", "symbol": "create" },
      "kind": "runtime"
    }
  ],
  "diagnostics": []
}
```

Plugins are never auto-loaded from dependencies. A plugin failure is a graph error.

## How it works

monoripple creates declaration nodes for each module using `oxc_semantic`. References inside a declaration become edges to the exact local or imported symbols they consume, while top-level execution is connected through a module-initialization node. Runtime and type-only edges are retained separately. Package exports, re-exports, literal dynamic imports, CommonJS loads, and imported assets link those nodes across files.

Affected traversal reverses the dependency graph:

```text
changed declaration
  -> local consumer
  -> imported binding
  -> application target
```

The base and current graphs are combined before traversal so removed declarations retain their previous consumer edges. Cycles terminate through visited sets and are reported as strongly connected components rather than rejected by default.

## Current boundaries

- pnpm v9 lockfile changes are modeled per workspace importer, including each importer’s resolved external dependency closure; changed library importers propagate through the existing runtime/type graph
- unsupported or malformed pnpm lockfiles, root/global settings, overrides, patches, ambiguous resolutions, and other lockfile formats conservatively affect every target and emit `MONORIPPLE_LOCKFILE_CHANGE_UNMODELED`
- non-Vite virtual entrypoints need explicit plugin roots for complete precision
- non-literal dynamic imports are diagnosed and should be promoted to errors for deployment planning
- deploy queries model source/configuration reachability, not final artifact hashes
