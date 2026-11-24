# OpenIDEA Style Compiler

English · [简体中文](https://github.com/wildcatDownstairs/openidea-style-compiler/blob/main/README.zh-CN.md)

A stylesheet compiler for [OpenIDEA](https://github.com/l0ng-ai/openidea): saving a Less, Sass/SCSS or
Stylus file compiles it to CSS, and compile errors show in the editor and the Problems view.

- Uses the project's own compiler: the nearest `node_modules/less`, `sass` or `stylus` above the file,
  run with `node`; the global `lessc` / `sass` / `stylus` when the project has none
- Saving a partial (Sass's `_vars.scss`, a `.less` / `.styl` file other files load) recompiles every
  file that loads it
- Errors are marked in the file they are in; Sass errors also point to where the file was loaded
- Projects built with Vite, webpack, Parcel, Next.js, Nuxt or Angular CLI are not compiled on save by
  default: those tools build their styles themselves
- Commands: _Style Compiler: Compile Current File_, _Style Compiler: Compile All Stylesheets_

Needs OpenIDEA 0.1.49 or later (plugin API 1.13).

## Permissions

| Permission | For |
| --- | --- |
| `project.read`, `events` (required) | Reading stylesheets and `package.json`, save events |
| `process:node` (optional) | Running the compiler in the project's `node_modules` (recommended) |
| `process:lessc`, `process:sass`, `process:stylus` (optional) | A global install, when the project has none |

Compilers run in trusted projects only. They write the CSS and source maps themselves, so the plugin
asks for no write access.

## Install

Search for Style Compiler in Settings › Plugins › Available, or download the `.oiplugin` from
[Releases](https://github.com/wildcatDownstairs/openidea-style-compiler/releases) and use
_Install Plugin from Package File…_.

## Settings

Settings › Plugins › Style Compiler; each can be set per project.

| Setting | Default | |
| --- | --- | --- |
| Compile on save | `auto` | `auto`: not in projects using the tools above; `always`; `off`. The commands compile in every mode |
| Compile Less / Sass/SCSS / Stylus on save | on | Per language |
| Output directory | empty | Relative to the project root, keeping folders (`src/a.scss` → `<dir>/src/a.css`); empty writes next to the source |
| Write source maps | off | |
| Minify the CSS | off | |
| Files not to compile | `node_modules/**` | One glob per line |

## Layout

```
plugin.toml           manifest
settings.schema.json  settings page
src/lib.rs            finding the compiler, command lines, error parsing, diagnostics
src/graph.rs          which stylesheets load which
dist/plugin.wasm      build output (not committed; `openidea-plugin build` before packing)
```

## Build and test

```bash
cargo test
```

```bash
openidea-plugin build    # → dist/plugin.wasm, checked as an install would
```

The compiler output in the tests is real output of less 4.9.1, sass 1.108.2 and stylus 0.64.0.

## Debugging in the IDE

```bash
openidea-plugin link .
```

`link` writes to the installed IDE's plugins directory; to link into a development build started with
`pnpm dev:app`, set `OPENIDEA_PLUGINS_DIR=<its data directory>/plugins`. After a change, run
`openidea-plugin build` again and _Reload_ in Settings › Plugins.

## Publishing

```bash
openidea-plugin build
```

```bash
openidea-plugin pack . --key <publisher key> --publisher wildcatdownstairs
```

Attach `wildcatdownstairs.style-compiler-<version>.oiplugin` to a GitHub Release, then add its
`[[versions]]` entry to [openidea-plugins](https://github.com/l0ng-ai/openidea-plugins) by pull request.

## Known limits

- `lessc` 4.9.1 cannot create more than one missing output folder. With the project's Less the plugin
  creates them first; with only a global `lessc`, the first compile into a new nested folder fails with `ENOENT`
- Sass deletes the previous CSS when a compile fails; Less and Stylus keep it
- Imports with variables or wildcards, and imports of packages in `node_modules`, are not tracked:
  changing those files doesn't trigger a recompile
- When two files load the same broken partial, its diagnostics are those of the last compile
