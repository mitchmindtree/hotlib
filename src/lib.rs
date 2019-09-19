//! For watching and loading a Rust library in Rust.
//!
//! See the [watch](./fn.watch.html) docs.

use derive_more::From;
use failure::Fail;
use notify::Watcher as NotifyWatcher;
use std::path::{Path, PathBuf};

/// Watches and re-builds the library upon changes to its source code.
pub struct Watch {
    cargo_config: cargo::Config,
    cargo_package: cargo::core::Package,
    cargo_toml_path: PathBuf,
    src_path: PathBuf,
    watcher: notify::RecommendedWatcher,
    event_rx: crossbeam_channel::Receiver<notify::RawEvent>,
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

/// Errors that might occur while waiting for the next loaded library instance.
#[derive(Debug, Fail, From)]
pub enum NextError {
    #[fail(display = "no dylib targets were found within the given cargo package")]
    NoDylibTarget,
    #[fail(display = "the channel used to receive file system events was closed")]
    ChannelClosed,
    #[fail(display = "a notify event signalled an error: {}", err)]
    Notify {
        #[fail(cause)]
        err: notify::Error,
    },
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
    let src_path = target.src_path().path().ok_or(WatchError::NoDylibTarget)?;

    // Begin watching the src path.
    let (tx, event_rx) = crossbeam_channel::unbounded();
    let mut watcher = notify::RecommendedWatcher::new_immediate(tx)?;
    watcher.watch(src_path, notify::RecursiveMode::Recursive)?;

    // Collect the paths.
    let cargo_toml_path = path.to_path_buf();
    let src_path = src_path.to_path_buf();

    Ok(Watch {
        cargo_config,
        cargo_package,
        cargo_toml_path,
        src_path,
        watcher,
        event_rx,
    })
}

// The first dylib target within the given package.
fn package_dylib_target(pkg: &cargo::core::Package) -> Option<&cargo::core::Target> {
    pkg.targets().iter().find(|t| t.is_dylib())
}

impl Watch {
    /// The path to the package's `Cargo.toml`.
    pub fn cargo_toml_path(&self) -> &Path {
        &self.cargo_toml_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.src_path
    }

    /// Wait for the library to be re-built after some change.
    pub fn next(&self) -> Result<libloading::Library, NextError> {
        loop {
            let event = match self.event_rx.recv() {
                Err(_) => return Err(NextError::ChannelClosed),
                Ok(event) => event,
            };
            if check_raw_event(event)? {
                return build_and_load(self);
            }
        }
    }

    /// The same as `next`, but returns early if there are no pending events.
    pub fn try_next(&self) -> Result<Option<libloading::Library>, NextError> {
        for event in self.event_rx.try_iter() {
            if check_raw_event(event)? {
                return build_and_load(self).map(Some);
            }
        }
        Ok(None)
    }

    /// Manually invoke a build of the watched library.
    ///
    /// This is useful for retrieving an initial build during model initialisation.
    pub fn build(&self) -> Result<libloading::Library, NextError> {
        build_and_load(self)
    }
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

// Attempt to build the watched library.
fn build_and_load(watch: &Watch) -> Result<libloading::Library, NextError> {
    let Watch {
        ref cargo_config,
        ref cargo_package,
        ..
    } = *watch;

    // Check there's a dylib target.
    let dylib_target = package_dylib_target(&cargo_package).ok_or(NextError::NoDylibTarget)?;

    // Compile the package.
    let options = compile_options(cargo_config).map_err(|err| NextError::Cargo { err })?;
    let pkg_manifest_path = cargo_package.manifest_path();
    println!("pre workspace");
    let pkg_workspace = cargo::core::Workspace::new(&pkg_manifest_path, &cargo_config)
        .map_err(|err| NextError::Cargo { err })?;
    println!("pre compilation");
    let compilation = cargo::ops::compile(&pkg_workspace, &options)
        .map_err(|err| NextError::CompilationFailed { err })?;

    // Locate the generated binary.
    let file_stem = format!("lib{}", dylib_target.name());
    let dylib_path = compilation.root_output.join(file_stem).with_extension(dylib_ext());
    println!("dylib path: {:?}", dylib_path);

    // Load the library and return it.
    println!("pre library load");
    let lib = libloading::Library::new(&dylib_path)?;
    Ok(lib)
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
