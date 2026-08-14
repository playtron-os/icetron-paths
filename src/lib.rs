//! XDG base-directory resolution, and lookup of native libraries bundled with an install.
//!
//! This is the one implementation of both, shared by every application in the fleet, and it
//! is deliberately dependency-free: a credential daemon or a file watcher must not acquire a
//! D-Bus stack or a GUI toolkit just to learn where its data lives.
//!
//! Shared-data lookups go through here instead of hardcoding `/usr/share`.
//! Three sources are consulted, most specific first:
//!
//! 1. the per-user dir (`XDG_DATA_HOME` / `XDG_CONFIG_HOME`, else
//!    `$HOME/.local/share` / `$HOME/.config`),
//! 2. the install prefix of the running executable (`<prefix>/bin/app` →
//!    `<prefix>/share`), so a relocatable install — a Nix store path, an
//!    `AppDir`, a `cargo install --root` tree — resolves its data with no
//!    environment set at all,
//! 3. every `XDG_DATA_DIRS` / `XDG_CONFIG_DIRS` entry, defaulting to the
//!    spec values `/usr/local/share:/usr/share` and `/etc/xdg` when unset —
//!    which is exactly the FHS behaviour that used to be hardcoded.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// XDG spec default for `XDG_DATA_DIRS`.
pub const DATA_DIRS_DEFAULT: &str = "/usr/local/share:/usr/share";

/// XDG spec default for `XDG_CONFIG_DIRS`.
pub const CONFIG_DIRS_DEFAULT: &str = "/etc/xdg";

/// Install prefix of the running executable, when it lives in `<prefix>/bin`.
///
/// Returns `None` when the executable is not in a `bin` directory (a `cargo
/// run` target dir, for instance) or when the path cannot be read.
#[must_use]
pub fn install_prefix() -> Option<PathBuf> {
    prefix_of(std::env::current_exe().ok()?.as_path())
}

/// Per-user data dir: `XDG_DATA_HOME`, else `$HOME/.local/share`.
#[must_use]
pub fn data_home() -> Option<PathBuf> {
    home_dir(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        ".local/share",
    )
}

/// Per-user config dir: `XDG_CONFIG_HOME`, else `$HOME/.config`.
#[must_use]
pub fn config_home() -> Option<PathBuf> {
    home_dir(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        ".config",
    )
}

/// Data roots to search, most specific first: [`data_home`], the install
/// prefix's `share`, then every `XDG_DATA_DIRS` entry (spec default
/// `/usr/local/share:/usr/share`). Duplicates are dropped.
#[must_use]
pub fn data_dirs() -> Vec<PathBuf> {
    resolve(
        data_home(),
        install_prefix().map(|p| p.join("share")),
        std::env::var_os("XDG_DATA_DIRS"),
        DATA_DIRS_DEFAULT,
    )
}

/// Config roots to search, most specific first: [`config_home`], the install
/// prefix's `etc/xdg`, then every `XDG_CONFIG_DIRS` entry (spec default
/// `/etc/xdg`). Duplicates are dropped.
#[must_use]
pub fn config_dirs() -> Vec<PathBuf> {
    resolve(
        config_home(),
        install_prefix().map(|p| p.join("etc/xdg")),
        std::env::var_os("XDG_CONFIG_DIRS"),
        CONFIG_DIRS_DEFAULT,
    )
}

/// Data roots for the *system* layer only, most specific first: the install prefix's `share`,
/// then every `XDG_DATA_DIRS` entry.
///
/// The per-user data dir is deliberately excluded. Some lookups are trust boundaries — a
/// caller allow-list, say — where a file under `$HOME` must not be able to outrank the one
/// the packager shipped. Use [`data_dirs`] when a user override is wanted, this when it is not.
#[must_use]
pub fn system_data_dirs() -> Vec<PathBuf> {
    resolve(
        None,
        install_prefix().map(|p| p.join("share")),
        std::env::var_os("XDG_DATA_DIRS"),
        DATA_DIRS_DEFAULT,
    )
}

/// [`system_data_dirs`] with `sub` appended to each root.
#[must_use]
pub fn system_data_subdirs(sub: &str) -> Vec<PathBuf> {
    system_data_dirs()
        .into_iter()
        .map(|d| d.join(sub))
        .collect()
}

/// Config roots for the *system* layer only, most specific first: the install prefix's
/// `etc/xdg`, then every `XDG_CONFIG_DIRS` entry.
///
/// The per-user config dir is deliberately excluded: callers that want it read the user layer
/// through their config backend, and mixing the two here would let a user file silently
/// outrank an admin one.
#[must_use]
pub fn system_config_dirs() -> Vec<PathBuf> {
    resolve(
        None,
        install_prefix().map(|p| p.join("etc/xdg")),
        std::env::var_os("XDG_CONFIG_DIRS"),
        CONFIG_DIRS_DEFAULT,
    )
}

/// [`config_dirs`] with `sub` appended to each root.
#[must_use]
pub fn config_subdirs(sub: &str) -> Vec<PathBuf> {
    config_dirs().into_iter().map(|d| d.join(sub)).collect()
}

/// [`data_dirs`] with `sub` appended to each root (e.g. `"applications"`).
#[must_use]
pub fn data_subdirs(sub: &str) -> Vec<PathBuf> {
    data_dirs().into_iter().map(|d| d.join(sub)).collect()
}

// ---------------------------------------------------------------------------------------
// Bundled native libraries (CEF, pdfium)
//
// These ship as loose shared libraries rather than as system packages with a stable soname on
// the loader path, so every embedder has to answer the same question at startup: where is the
// distribution? The answer is a property of how the application was installed, which is why it
// belongs next to the XDG lookup rather than being re-derived in each application.
// Absent on wasm: there is no dynamic library to find, and no filesystem to find it on.
// ---------------------------------------------------------------------------------------

#[cfg(not(target_family = "wasm"))]
mod native {
    use super::{prefix_of, push_unique, DATA_DIRS_DEFAULT};
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    /// File whose presence identifies a directory as a CEF distribution.
    #[cfg(target_os = "linux")]
    pub const CEF_LIB: &str = "libcef.so";
    #[cfg(target_os = "macos")]
    pub const CEF_LIB: &str = "Chromium Embedded Framework";
    #[cfg(target_os = "windows")]
    pub const CEF_LIB: &str = "libcef.dll";

    /// Where the `cef-libs` package puts the distribution on FHS systems.
    pub const CEF_FHS_DIR: &str = "/usr/lib/cef";

    /// Environment variable the `cef` crate reads to locate the distribution.
    pub const CEF_PATH_ENV: &str = "CEF_PATH";

    /// Where a CEF distribution sits under an install prefix.
    const CEF_SUBDIR: &str = "lib/cef";

    /// Library directories to try under an install prefix, in order.
    ///
    /// Both spellings are needed: Debian-family and Nix use `lib`, Fedora-family uses `lib64`.
    pub const LIB_SUBDIRS: [&str; 2] = ["lib", "lib64"];

    /// The environment a CEF lookup depends on, captured so the search is testable.
    #[derive(Debug, Default, Clone)]
    pub struct CefSearch {
        /// The running executable, used to derive the install prefix.
        pub exe: Option<PathBuf>,
        /// `$CEF_PATH` — an explicit override, tried first.
        pub cef_path: Option<OsString>,
        /// `$XDG_DATA_DIRS` — covers a CEF that lives in its own prefix (its own Nix store path).
        pub xdg_data_dirs: Option<OsString>,
        /// `$LD_LIBRARY_PATH` — how a development run points at a build-tree CEF.
        pub ld_library_path: Option<OsString>,
    }

    impl CefSearch {
        /// Capture the search inputs from the current process.
        #[must_use]
        pub fn from_env() -> Self {
            Self {
                exe: std::env::current_exe().ok(),
                cef_path: std::env::var_os(CEF_PATH_ENV),
                xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS"),
                ld_library_path: std::env::var_os("LD_LIBRARY_PATH"),
            }
        }

        /// Directories that may hold a CEF distribution, most specific first.
        ///
        /// An explicit `CEF_PATH` wins, then the prefix the binary was installed into, then each
        /// `XDG_DATA_DIRS` entry's prefix (`/usr/share` -> `/usr/lib/cef`, and on Nix a CEF in its
        /// own store path), then `LD_LIBRARY_PATH`, and finally the FHS location — so an existing
        /// FHS install resolves exactly where it always did.
        #[must_use]
        pub fn candidates(&self) -> Vec<PathBuf> {
            let mut dirs: Vec<PathBuf> = Vec::new();

            if let Some(path) = self.cef_path.as_deref().filter(|p| !p.is_empty()) {
                push_unique(&mut dirs, PathBuf::from(path));
            }
            if let Some(prefix) = self.exe.as_deref().and_then(prefix_of) {
                push_unique(&mut dirs, prefix.join(CEF_SUBDIR));
            }

            let data_dirs = self
                .xdg_data_dirs
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| DATA_DIRS_DEFAULT.into());
            for root in std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()) {
                if let Some(prefix) = root.parent() {
                    push_unique(&mut dirs, prefix.join(CEF_SUBDIR));
                }
            }

            if let Some(path) = self.ld_library_path.as_deref() {
                for dir in std::env::split_paths(path) {
                    push_unique(&mut dirs, dir);
                }
            }
            push_unique(&mut dirs, PathBuf::from(CEF_FHS_DIR));

            dirs
        }

        /// The first candidate that actually contains a CEF distribution.
        #[must_use]
        pub fn find(&self) -> Option<PathBuf> {
            self.find_with(&|dir: &Path| dir.join(CEF_LIB).is_file())
        }

        /// [`Self::find`] with the "is this a CEF directory?" test injected, so a test does not
        /// depend on what happens to be installed on the machine running it.
        #[must_use]
        pub fn find_with(&self, is_cef_dir: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
            self.candidates().into_iter().find(|dir| is_cef_dir(dir))
        }
    }

    /// The first directory containing a CEF distribution, searched from the current environment.
    #[must_use]
    pub fn find_cef_dir() -> Option<PathBuf> {
        CefSearch::from_env().find()
    }

    /// Locate CEF and export [`CEF_PATH_ENV`] so the `cef` crate finds the same distribution.
    ///
    /// Returns `None` when nothing was found, leaving the environment alone — which matters for a
    /// binary run out of a build tree, where `cef-dll-sys` falls back to the copy in its `OUT_DIR`
    /// and overwriting `CEF_PATH` would take that away.
    ///
    /// A `CEF_PATH` that is already set and valid is returned untouched; one pointing at nothing is
    /// treated as stale and the search continues, since falling through beats refusing to start.
    ///
    /// Mutates the process environment, so **call this from `main` before starting any threads**.
    pub fn ensure_cef_path() -> Option<PathBuf> {
        let search = CefSearch::from_env();
        let resolved = search.find()?;

        if search.cef_path.as_deref() != Some(resolved.as_os_str()) {
            std::env::set_var(CEF_PATH_ENV, &resolved);
        }

        Some(resolved)
    }

    /// Candidate paths for a native library file, most specific first.
    ///
    /// `override_path` (typically an application-specific environment variable) may name either the
    /// library file itself or the directory holding it. The working directory is included so a dev
    /// run can drop the library beside the build tree, and the install prefix is expanded over
    /// [`LIB_SUBDIRS`].
    ///
    /// Callers should keep a bare-soname `dlopen` as the final fallback: where the distribution
    /// packages the library properly, the loader already knows where it is.
    #[must_use]
    pub fn library_candidates(
        lib_name: &OsStr,
        override_path: Option<&OsStr>,
        exe: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        if let Some(path) = override_path.filter(|p| !p.is_empty()).map(PathBuf::from) {
            // Accept the library file itself as well as the directory holding it.
            if path.file_name() == Some(lib_name) {
                push_unique(&mut candidates, path);
            } else {
                push_unique(&mut candidates, path.join(lib_name));
            }
        }

        push_unique(&mut candidates, PathBuf::from(".").join(lib_name));

        if let Some(prefix) = exe.and_then(prefix_of) {
            for sub in LIB_SUBDIRS {
                push_unique(&mut candidates, prefix.join(sub).join(lib_name));
            }
        }

        candidates
    }

    /// [`library_candidates`] resolved against the current process, honouring `env_var` as the
    /// override. Returns the first path that exists.
    #[must_use]
    pub fn find_library(lib_name: &OsStr, env_var: &str) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok();
        let override_path = std::env::var_os(env_var);

        library_candidates(lib_name, override_path.as_deref(), exe.as_deref())
            .into_iter()
            .find(|p| p.exists())
    }
}

#[cfg(not(target_family = "wasm"))]
pub use native::*;

/// `<prefix>` for an executable path `<prefix>/bin/<exe>`.
///
/// Split out from [`install_prefix`] so it is testable without running a
/// binary from a fixture prefix.
#[must_use]
pub fn prefix_of(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    bin.parent().map(Path::to_path_buf)
}

/// Per-user dir: `xdg_home` verbatim when set, else `$HOME` joined with
/// `fallback`.
///
/// Env values arrive as parameters so tests never mutate the process env.
fn home_dir(xdg_home: Option<OsString>, home: Option<OsString>, fallback: &str) -> Option<PathBuf> {
    if let Some(dir) = xdg_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(fallback))
}

/// Merge the three sources into one deduplicated, ordered search list.
///
/// Env values arrive as parameters so tests never mutate the process env.
fn resolve(
    home: Option<PathBuf>,
    prefix: Option<PathBuf>,
    dirs_var: Option<OsString>,
    dirs_default: &str,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in home.into_iter().chain(prefix) {
        push_unique(&mut out, dir);
    }
    let dirs = dirs_var
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| dirs_default.into());
    for dir in std::env::split_paths(&dirs) {
        push_unique(&mut out, dir);
    }
    out
}

fn push_unique(out: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dir.as_os_str().is_empty() && !out.contains(&dir) {
        out.push(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system tier must never include the per-user dir: lookups that are trust
    /// boundaries rely on a `$HOME` file being unable to outrank the packaged one.
    #[test]
    fn system_data_dirs_excludes_the_user_tier() {
        let user = resolve(
            Some(PathBuf::from("/home/u/.local/share")),
            Some(PathBuf::from("/usr/share")),
            None,
            DATA_DIRS_DEFAULT,
        );
        let system = resolve(
            None,
            Some(PathBuf::from("/usr/share")),
            None,
            DATA_DIRS_DEFAULT,
        );
        assert!(user.contains(&PathBuf::from("/home/u/.local/share")));
        assert!(!system.contains(&PathBuf::from("/home/u/.local/share")));
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn prefix_is_the_parent_of_bin() {
        assert_eq!(
            prefix_of(Path::new("/nix/store/abc123-icetron-app/bin/app")),
            Some(PathBuf::from("/nix/store/abc123-icetron-app"))
        );
        assert_eq!(
            prefix_of(Path::new("/usr/bin/app")),
            Some(PathBuf::from("/usr"))
        );
    }

    #[test]
    fn prefix_is_none_outside_a_bin_dir() {
        // `cargo run` layout: no prefix to derive.
        assert_eq!(prefix_of(Path::new("/src/app/target/debug/app")), None);
        assert_eq!(prefix_of(Path::new("app")), None);
    }

    #[test]
    fn fedora_defaults_match_the_old_hardcoded_paths() {
        // Nothing but $HOME set — the exact behaviour of the literals this
        // module replaced, plus the per-user dir the spec mandates.
        let dirs = resolve(
            home_dir(None, Some(os("/home/user")), ".local/share"),
            None,
            None,
            DATA_DIRS_DEFAULT,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/user/.local/share"),
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        );
    }

    #[test]
    fn fedora_install_prefix_does_not_duplicate_usr_share() {
        let dirs = resolve(
            home_dir(None, Some(os("/home/user")), ".local/share"),
            Some(PathBuf::from("/usr/share")),
            None,
            DATA_DIRS_DEFAULT,
        );
        assert_eq!(
            dirs.iter()
                .filter(|d| d.as_path() == Path::new("/usr/share"))
                .count(),
            1
        );
    }

    #[test]
    fn nixos_store_paths_resolve_from_xdg_data_dirs() {
        // NixOS never has /usr/share; profiles are surfaced via XDG_DATA_DIRS.
        let dirs = resolve(
            home_dir(None, Some(os("/home/user")), ".local/share"),
            Some(PathBuf::from("/nix/store/abc-app/share")),
            Some(os(
                "/run/current-system/sw/share:/home/user/.nix-profile/share",
            )),
            DATA_DIRS_DEFAULT,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/user/.local/share"),
                PathBuf::from("/nix/store/abc-app/share"),
                PathBuf::from("/run/current-system/sw/share"),
                PathBuf::from("/home/user/.nix-profile/share"),
            ]
        );
        assert!(!dirs.contains(&PathBuf::from("/usr/share")));
    }

    #[test]
    fn nixos_bare_store_binary_still_finds_its_own_data() {
        // No env at all: the exe-relative prefix is the only thing that works.
        let dirs = resolve(
            None,
            Some(PathBuf::from("/nix/store/abc-app/share")),
            None,
            DATA_DIRS_DEFAULT,
        );
        assert_eq!(
            dirs.first(),
            Some(&PathBuf::from("/nix/store/abc-app/share"))
        );
    }

    #[test]
    fn xdg_data_home_overrides_the_home_join() {
        assert_eq!(
            home_dir(
                Some(os("/home/user/.xdgdata")),
                Some(os("/home/user")),
                ".local/share"
            ),
            Some(PathBuf::from("/home/user/.xdgdata"))
        );
        assert_eq!(
            home_dir(
                Some(OsString::new()),
                Some(os("/home/user")),
                ".local/share"
            ),
            Some(PathBuf::from("/home/user/.local/share"))
        );
        assert_eq!(home_dir(None, None, ".local/share"), None);
    }

    #[test]
    fn config_defaults_to_etc_xdg_and_keeps_profile_entries() {
        assert_eq!(
            resolve(None, None, None, CONFIG_DIRS_DEFAULT),
            vec![PathBuf::from("/etc/xdg")]
        );
        assert_eq!(
            resolve(
                None,
                None,
                Some(os("/etc/xdg:/nix/store/abc-app/etc/xdg")),
                CONFIG_DIRS_DEFAULT
            ),
            vec![
                PathBuf::from("/etc/xdg"),
                PathBuf::from("/nix/store/abc-app/etc/xdg"),
            ]
        );
    }

    #[test]
    fn empty_entries_are_dropped() {
        assert_eq!(
            resolve(None, None, Some(os("/a::/b:/a")), DATA_DIRS_DEFAULT),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}

/// The bundled-native-library lookup, which has to work on an FHS distribution and on one
/// where nothing lives under `/usr` at all.
#[cfg(all(test, not(target_family = "wasm")))]
mod native_lib_tests {
    use super::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    /// Predicate over a fixed set of directories that "contain" a CEF distribution, so the
    /// search is tested against a known layout rather than whatever the host has installed.
    fn present(dirs: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |path: &Path| dirs.iter().any(|d| Path::new(d) == path)
    }

    fn search(exe: &str) -> CefSearch {
        CefSearch {
            exe: Some(PathBuf::from(exe)),
            ..CefSearch::default()
        }
    }

    /// FHS: the prefix candidate and the XDG default both land on the directory the `cef-libs`
    /// package installs into, so behaviour there is exactly what it was.
    #[test]
    fn fhs_resolves_to_the_usual_cef_dir() {
        let found = search("/usr/bin/app").find_with(&present(&["/usr/lib/cef"]));
        assert_eq!(found, Some(PathBuf::from("/usr/lib/cef")));
    }

    /// A relocated install with CEF in its own tree resolves with no environment at all.
    #[test]
    fn store_prefix_resolves_without_env() {
        let found = search("/nix/store/abc-app/bin/app")
            .find_with(&present(&["/nix/store/abc-app/lib/cef"]));
        assert_eq!(found, Some(PathBuf::from("/nix/store/abc-app/lib/cef")));
    }

    /// CEF packaged separately, reachable only through `XDG_DATA_DIRS`. Two of the four
    /// per-application copies this crate replaces never looked here.
    #[test]
    fn cef_in_its_own_prefix_via_xdg_data_dirs() {
        let s = CefSearch {
            exe: Some(PathBuf::from("/nix/store/abc-app/bin/app")),
            xdg_data_dirs: Some(os("/nix/store/xyz-cef/share:/run/current-system/sw/share")),
            ..CefSearch::default()
        };
        assert_eq!(
            s.find_with(&present(&["/nix/store/xyz-cef/lib/cef"])),
            Some(PathBuf::from("/nix/store/xyz-cef/lib/cef"))
        );
    }

    #[test]
    fn ld_library_path_is_searched() {
        let s = CefSearch {
            exe: Some(PathBuf::from("/usr/bin/app")),
            ld_library_path: Some(os("/build/cef")),
            ..CefSearch::default()
        };
        assert_eq!(
            s.find_with(&present(&["/build/cef"])),
            Some(PathBuf::from("/build/cef"))
        );
    }

    #[test]
    fn explicit_cef_path_outranks_everything() {
        let s = CefSearch {
            exe: Some(PathBuf::from("/usr/bin/app")),
            cef_path: Some(os("/opt/my-cef")),
            ld_library_path: Some(os("/build/cef")),
            ..CefSearch::default()
        };
        assert_eq!(s.candidates().first(), Some(&PathBuf::from("/opt/my-cef")));
    }

    /// A `CEF_PATH` pointing at nothing must not veto the search: failing to start the webview
    /// over a stale override is worse than falling through to a working directory.
    #[test]
    fn stale_cef_path_falls_through() {
        let s = CefSearch {
            exe: Some(PathBuf::from("/usr/bin/app")),
            cef_path: Some(os("/gone/cef")),
            ..CefSearch::default()
        };
        assert_eq!(
            s.find_with(&present(&["/usr/lib/cef"])),
            Some(PathBuf::from("/usr/lib/cef"))
        );
    }

    /// The FHS dir is reachable three ways; it must be probed once, at its highest position.
    #[test]
    fn duplicate_candidates_collapse() {
        let s = CefSearch {
            exe: Some(PathBuf::from("/usr/bin/app")),
            cef_path: Some(os("/usr/lib/cef")),
            ..CefSearch::default()
        };
        let dirs = s.candidates();
        assert_eq!(
            dirs.iter()
                .filter(|d| *d == &PathBuf::from("/usr/lib/cef"))
                .count(),
            1
        );
        assert_eq!(dirs.first(), Some(&PathBuf::from("/usr/lib/cef")));
    }

    /// Nothing found must be distinguishable, so a build-tree binary can keep its own copy.
    #[test]
    fn nothing_found_is_none() {
        assert_eq!(search("/usr/bin/app").find_with(&present(&[])), None);
    }

    #[test]
    fn library_override_accepts_a_file_or_a_directory() {
        let name = OsString::from("libpdfium.so");

        let as_dir = os("/opt/pdfium");
        assert_eq!(
            library_candidates(&name, Some(&as_dir), None).first(),
            Some(&PathBuf::from("/opt/pdfium/libpdfium.so"))
        );

        let as_file = os("/opt/pdfium/libpdfium.so");
        assert_eq!(
            library_candidates(&name, Some(&as_file), None).first(),
            Some(&PathBuf::from("/opt/pdfium/libpdfium.so"))
        );
    }

    /// Fedora ships pdfium in `/usr/lib64`, Debian and Nix in `lib`. Both are probed.
    #[test]
    fn library_candidates_cover_both_lib_subdirs() {
        let name = OsString::from("libpdfium.so");
        let dirs = library_candidates(&name, None, Some(Path::new("/usr/bin/viewer")));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("./libpdfium.so"),
                PathBuf::from("/usr/lib/libpdfium.so"),
                PathBuf::from("/usr/lib64/libpdfium.so"),
            ]
        );
    }

    /// A `cargo run` build has no sibling `lib/`, so only the working directory is probed.
    #[test]
    fn build_tree_binary_has_no_prefix_candidates() {
        let name = OsString::from("libpdfium.so");
        let dirs = library_candidates(&name, None, Some(Path::new("/home/dev/target/debug/app")));
        assert_eq!(dirs, vec![PathBuf::from("./libpdfium.so")]);
    }
}
