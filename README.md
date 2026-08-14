# icetron-paths

Locate an application's data and its bundled native libraries relative to the running
executable, so the same binary works on an FHS distribution and on one where nothing lives
under `/usr` (Nix, a relocated tarball, an `AppDir`, `cargo install --root`).

Dependency-free and `std`-only. A credential daemon or a file watcher can use it without
acquiring a GUI toolkit, a D-Bus stack or a filesystem watcher.

```rust
// Data shipped with the package: <prefix>/share/<app>, $XDG_DATA_HOME, $XDG_DATA_DIRS
let dirs = icetron_paths::data_subdirs("myapp/templates");

// A trust boundary — same list without the user-writable tier
let dirs = icetron_paths::system_data_subdirs("myapp/allowed-callers.d");

// A bundled native library (CEF, pdfium)
icetron_paths::ensure_cef_path();
let lib = icetron_paths::find_library(OsStr::new("libpdfium.so"), "PDFIUM_LIB_PATH");
```

## Why it exists

The rule `<prefix>/bin/app` implies `<prefix>/share` resolves a relocated install with no
environment set at all, which is what a hardcoded `/usr/share` cannot do. The XDG spec
defaults are the historical FHS paths (`/usr/local/share:/usr/share`, `/etc/xdg`), so moving a
lookup onto this crate leaves FHS behaviour byte-identical.

It was extracted after the same ~60 lines were found reimplemented in fourteen repositories,
having drifted between them in probe order and in which environment variables they honoured.

## Two things worth knowing

**Ordering.** Every list is returned **most specific first**. Callers that merge in order and
let *later* entries win must reverse it, or precedence flips silently.

**Trust boundaries.** `data_dirs` includes `$XDG_DATA_HOME`; `system_data_dirs` does not. For
anything that authorizes — an allow-list of executables, say — use the `system_` variants, or a
file under `$HOME` can grant privileges the packager never granted.

## Platform support

The XDG half builds everywhere, including `wasm32`. The native-library half (CEF, pdfium) is
absent on wasm, where there is no dynamic library to find and no filesystem to find it on.

## License

MIT
