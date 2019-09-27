//! For watching and loading a Rust library in Rust.
//!
//! You are likely looking for the [watch function docs](./fn.watch.html).

use derive_more::From;
use failure::Fail;
use notify::Watcher as NotifyWatcher;
use slug::slugify;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[doc(inline)]
pub use libloading::{self, Library, Symbol};

/// Watches and re-builds the library upon changes to its source code.
pub struct Watch {
    package_info: PackageInfo,
    watcher: notify::RecommendedWatcher,
    event_rx: crossbeam_channel::Receiver<notify::RawEvent>,
}

struct PackageInfo {
    cargo_config: cargo::Config,
    cargo_package: cargo::core::Package,
    cargo_toml_path: PathBuf,
    src_path: PathBuf,
}

/// The information required to build the package's dylib target.
pub struct Package<'a> {
    info: &'a PackageInfo,
}

/// The result of building a package's dynamic library.
///
/// This can be used to load the dynamic library either in place or via a temporary file as to allow
/// for re-building the package while using the library.
pub struct Build<'a> {
    timestamp: SystemTime,
    target: &'a cargo::core::manifest::Target,
    compilation: cargo::core::compiler::Compilation<'a>,
}

/// A wrapper around a `libloading::Library` that cleans up the library on `Drop`.
pub struct TempLibrary {
    build_timestamp: SystemTime,
    path: PathBuf,
    // This is always `Some`. An `Option` is only used so that the library may be `Drop`ped during
    // the `TempLibrary`'s `drop` implementation before the temporary library file at `path` is
    // removed.
    lib: Option<libloading::Library>,
}

/// Errors that might occur within the `watch` function.
#[derive(Debug, Fail, From)]
pub enum WatchError {
    #[fail(display = "invalid path: expected path to end with `Cargo.toml`")]
    InvalidPath,
    #[fail(display = "a Cargo error occurred: {}", err)]
    Cargo {
        #[fail(cause)]
        err: failure::Error,
    },
    #[fail(display = "no dylib targets were found within the given cargo package")]
    NoDylibTarget,
    #[fail(display = "failed to construct `notify::RecommendedWatcher`: {}", err)]
    Notify {
        #[fail(cause)]
        err: notify::Error,
    },
}

/// Errors that might occur while building a library instance.
#[derive(Debug, Fail, From)]
pub enum BuildError {
    #[fail(display = "no dylib targets were found within the given cargo package")]
    NoDylibTarget,
    #[fail(display = "an error occurred within cargo: {}", err)]
    Cargo {
        #[fail(cause)]
        err: failure::Error,
    },
    #[fail(display = "compilation failed: {}", err)]
    CompilationFailed {
        #[fail(cause)]
        err: failure::Error,
    },
    #[fail(display = "failed to load dynamic library: {}", err)]
    LoadingLibraryFailed {
        #[fail(cause)]
        err: std::io::Error,
    }
}

/// Errors that might occur while waiting for the next library instance.
#[derive(Debug, Fail, From)]
pub enum NextError {
    #[fail(display = "the channel used to receive file system events was closed")]
    ChannelClosed,
    #[fail(display = "a notify event signalled an error: {}", err)]
    Notify {
        #[fail(cause)]
        err: notify::Error,
    },
}

/// Errors that might occur while loading a built library.
#[derive(Debug, Fail)]
pub enum LoadError {
    #[fail(display = "an IO error occurred: {}", err)]
    Io {
        #[fail(cause)]
        err: std::io::Error,
    },
    #[fail(display = "failed to load library with libloading: {}", err)]
    Library {
        #[fail(cause)]
        err: std::io::Error,
    },
}

/// Watch the library at the given `Path`.
///
/// The given `Path` should point to the `Cargo.toml` of the package used to build the library.
///
/// When a library is being "watched", the library will be re-built any time some filesystem event
/// occurs within the library's source directory. The target used is the first "dylib" discovered
/// within the package.
///
/// The `notify` crate is used to watch for file-system events in a cross-platform manner.
pub fn watch(path: &Path) -> Result<Watch, WatchError> {
    if !path.ends_with("Cargo.toml") && !path.ends_with("cargo.toml") {
        return Err(WatchError::InvalidPath);
    }

    // Find the package's dylib target src directory.
    let cargo_config = cargo::Config::default()?;
    let source_id = cargo::core::SourceId::for_path(path)?;
    let (cargo_package, _) = cargo::ops::read_package(path, source_id, &cargo_config)?;
    let target = package_dylib_target(&cargo_package).ok_or(WatchError::NoDylibTarget)?;
    let src_root_path = target.src_path().path().ok_or(WatchError::NoDylibTarget)?;
    let src_dir_path = src_root_path.parent().expect("src root has no parent directory");

    // Begin watching the src path.
    let (tx, event_rx) = crossbeam_channel::unbounded();
    let mut watcher = notify::RecommendedWatcher::new_immediate(tx)?;
    watcher.watch(src_dir_path, notify::RecursiveMode::Recursive)?;

    // Collect the paths.
    let cargo_toml_path = path.to_path_buf();
    let src_path = src_dir_path.to_path_buf();

    // Collect the package info.
    let package_info = PackageInfo {
        cargo_config,
        cargo_package,
        cargo_toml_path,
        src_path,
    };

    Ok(Watch {
        package_info,
        watcher,
        event_rx,
    })
}

impl Watch {
    /// The path to the package's `Cargo.toml`.
    pub fn cargo_toml_path(&self) -> &Path {
        &self.package_info.cargo_toml_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.package_info.src_path
    }

    /// Wait for the library to be re-built after some change.
    pub fn next(&self) -> Result<Package, NextError> {
        loop {
            let event = match self.event_rx.recv() {
                Err(_) => return Err(NextError::ChannelClosed),
                Ok(event) => event,
            };
            if check_raw_event(event)? {
                return Ok(self.package());
            }
        }
    }

    /// The same as `next`, but returns early if there are no pending events.
    pub fn try_next(&self) -> Result<Option<Package>, NextError> {
        for event in self.event_rx.try_iter() {
            if check_raw_event(event)? {
                return Ok(Some(self.package()));
            }
        }
        Ok(None)
    }

    /// Manually retrieve the library's package immediately without checking for file events.
    ///
    /// This is useful for triggering an initial build during model initialisation.
    pub fn package(&self) -> Package {
        let info = &self.package_info;
        Package { info }
    }
}

impl<'a> Package<'a> {
    /// The path to the package's `Cargo.toml`.
    pub fn cargo_toml_path(&self) -> &Path {
        &self.info.cargo_toml_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.info.src_path
    }

    /// Builds the package's dynamic library target.
    pub fn build(&self) -> Result<Build<'a>, BuildError> {
        let PackageInfo {
            ref cargo_package,
            ref cargo_config,
            ..
        } = self.info;

        // Check there's a dylib target.
        let target = package_dylib_target(cargo_package).ok_or(BuildError::NoDylibTarget)?;

        // Compile the package.
        let options = compile_options(cargo_config).map_err(|err| BuildError::Cargo { err })?;
        let pkg_manifest_path = cargo_package.manifest_path();
        let pkg_workspace = cargo::core::Workspace::new(&pkg_manifest_path, cargo_config)
            .map_err(|err| BuildError::Cargo { err })?;
        let compilation = cargo::ops::compile(&pkg_workspace, &options)
            .map_err(|err| BuildError::CompilationFailed { err })?;
        let timestamp = SystemTime::now();

        Ok(Build { timestamp, target, compilation })
    }
}

impl<'a> Build<'a> {
    /// The moment at which the build was completed.
    pub fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    /// The path to the generated dylib target.
    pub fn dylib_path(&self) -> PathBuf {
        let file_stem = self.file_stem();
        self.compilation.root_output.join(file_stem).with_extension(dylib_ext())
    }

    /// The path to the temporary dynamic library clone that will be created upon `load`.
    pub fn tmp_dylib_path(&self) -> PathBuf {
        tmp_dir().join(self.tmp_file_stem()).with_extension(dylib_ext())
    }

    /// Copy the library to the platform's temporary directory and load it from there.
    ///
    /// Note that the copied dynamic library will be removed on `Drop`.
    pub fn load(self) -> Result<TempLibrary, LoadError> {
        let dylib_path = self.dylib_path();
        let tmp_path = self.tmp_dylib_path();

        // If the library already exists, load it.
        loop {
            if tmp_path.exists() {
                let lib = libloading::Library::new(&tmp_path)
                    .map(Some)
                    .map_err(|err| LoadError::Library { err })?;
                let path = tmp_path;
                let build_timestamp = self.timestamp;
                let tmp = TempLibrary { build_timestamp, path, lib };
                return Ok(tmp);
            }

            // Copy the dylib to the tmp location.
            let tmp_dir = tmp_path.parent().expect("temp dylib path has no parent");
            std::fs::create_dir_all(tmp_dir).map_err(|err| LoadError::Io { err })?;
            std::fs::copy(&dylib_path, &tmp_path).map_err(|err| LoadError::Io { err })?;
        }
    }

    /// Load the library from it's existing location.
    ///
    /// Note that if you do this, you will have to ensure the returned `Library` is dropped before
    /// attempting to re-build the library.
    pub fn load_in_place(self) -> libloading::Result<libloading::Library> {
        let dylib_path = self.dylib_path();
        libloading::Library::new(dylib_path)
    }

    // The file stem of the built dynamic library.
    fn file_stem(&self) -> String {
        // TODO: On windows, the generated lib does not contain the "lib" prefix.
        // A proper solution would likely involve retrieving the file stem from cargo itself.
        #[cfg(target_os = "windows")]
        {
            format!("{}", self.target.name())
        }
        #[cfg(not(target_os = "windows"))]
        {
            format!("lib{}", self.target.name())
        }
    }

    // Produce the file stem for the temporary dynamic library clone that will be created upon
    // `load`.
    fn tmp_file_stem(&self) -> String {
        let timestamp_slug = slugify(format!("{}", humantime::format_rfc3339(self.timestamp)));
        format!("{}-{}", self.file_stem(), timestamp_slug)
    }
}

impl TempLibrary {
    /// The inner `libloading::Library`.
    ///
    /// This may also be accessed via the `Deref` implementation.
    pub fn lib(&self) -> &libloading::Library {
        self.lib.as_ref().expect("lib should always be `Some` until `Drop`")
    }

    /// The time at which the original library was built.
    pub fn build_timestamp(&self) -> SystemTime {
        self.build_timestamp
    }

    /// The path at which the loaded temporary library is located.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempLibrary {
    type Target = libloading::Library;
    fn deref(&self) -> &Self::Target {
        self.lib()
    }
}

impl Drop for TempLibrary {
    fn drop(&mut self) {
        std::mem::drop(self.lib.take());
        std::fs::remove_file(&self.path).ok();
    }
}

// The first dylib target within the given package.
fn package_dylib_target(pkg: &cargo::core::Package) -> Option<&cargo::core::Target> {
    pkg.targets().iter().find(|t| t.is_dylib())
}

// The temporary directory used by hotlib.
fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join("hotlib")
}

// Whether or not the given event should trigger a rebuild.
fn _check_event(_event: notify::Event) -> bool {
    true
}

// Whether or not the given event should trigger a rebuild.
fn check_raw_event(event: notify::RawEvent) -> Result<bool, NextError> {
    use notify::Op;
    Ok(event
        .op?
        .intersects(Op::CREATE | Op::REMOVE | Op::WRITE | Op::CLOSE_WRITE | Op::RENAME | Op::METADATA))
}

// Get the dylib extension for this platform.
//
// TODO: This should be exposed from cargo.
fn dylib_ext() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        return "so";
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        return "dylib";
    }
    #[cfg(target_os = "windows")]
    {
        return "dll";
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        panic!("unknown dynamic library for this platform")
    }
}

// The default compile options used for building the dynamic library.
fn compile_options(conf: &cargo::Config) -> Result<cargo::ops::CompileOptions, failure::Error> {
    let mode = cargo::core::compiler::CompileMode::Build;
    let mut opts = cargo::ops::CompileOptions::new(conf, mode)?;
    opts.build_config.release = true;
    Ok(opts)
}
