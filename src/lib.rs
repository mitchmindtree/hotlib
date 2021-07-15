//! For watching and loading a Rust library in Rust.
//!
//! You are likely looking for the [watch function docs](./fn.watch.html).

use notify::Watcher as NotifyWatcher;
use slug::slugify;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;
use thiserror::Error;

#[doc(inline)]
pub use libloading::{self, Library, Symbol};

/// Watches and re-builds the library upon changes to its source code.
pub struct Watch {
    package_info: PackageInfo,
    _watcher: notify::RecommendedWatcher,
    event_rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

struct PackageInfo {
    manifest_path: PathBuf,
    src_path: PathBuf,
    lib_name: String,
    target_dir_path: PathBuf,
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
    lib_name: &'a str,
    target_dir_path: &'a Path,
    timestamp: SystemTime,
    output: std::process::Output,
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
#[derive(Debug, Error)]
pub enum WatchError {
    #[error("invalid path: expected path to end with `Cargo.toml`")]
    InvalidPath,
    #[error("an IO error occurred while attempting to invoke `cargo metadata`: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },
    #[error("{err}")]
    ExitStatusUnsuccessful {
        #[from]
        err: ExitStatusUnsuccessfulError,
    },
    #[error("an error occurred when attempting to read cargo stdout as json: {err}")]
    Json {
        #[from]
        err: serde_json::Error,
    },
    #[error("no dylib targets were found within the given cargo package")]
    NoDylibTarget,
    #[error("failed to construct `notify::RecommendedWatcher`: {err}")]
    Notify {
        #[from]
        err: notify::Error,
    },
}

/// Errors that might occur while building a library instance.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("an IO error occurred while attempting to invoke cargo: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },
    #[error("{err}")]
    ExitStatusUnsuccessful {
        #[from]
        err: ExitStatusUnsuccessfulError,
    },
}

/// A process' output indicates unsuccessful completion.
#[derive(Debug, Error)]
#[error("cargo process exited unsuccessfully with status code: {code:?}: {stderr}")]
pub struct ExitStatusUnsuccessfulError {
    pub code: Option<i32>,
    pub stderr: String,
}

/// Errors that might occur while waiting for the next library instance.
#[derive(Debug, Error)]
pub enum NextError {
    #[error("the channel used to receive file system events was closed")]
    ChannelClosed,
    #[error("a notify event signalled an error: {err}")]
    Notify {
        #[from]
        err: notify::Error,
    },
}

/// Errors that might occur while loading a built library.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("an IO error occurred: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },
    #[error("failed to load library with libloading: {err}")]
    Library {
        #[from]
        err: libloading::Error,
    },
}

impl ExitStatusUnsuccessfulError {
    /// Produces the error if output indicates failure.
    pub fn from_output(output: &std::process::Output) -> Option<Self> {
        // Check for process failure.
        if !output.status.success() {
            let code = output.status.code();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Some(ExitStatusUnsuccessfulError { code, stderr });
        }
        None
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

    // Run the `cargo metadata` command to retrieve JSON containing lib target info.
    let manifest_path_str = format!("{}", path.display());
    let output = std::process::Command::new("cargo")
        .arg("metadata")
        .arg("--manifest-path")
        .arg(&manifest_path_str)
        .arg("--format-version")
        .arg("1")
        .output()?;

    // Check the exit status.
    if let Some(err) = ExitStatusUnsuccessfulError::from_output(&output) {
        return Err(WatchError::from(err));
    }

    // Read the stdout as JSON.
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    // A function to read paths and name out of JSON.
    let read_json = |json: &serde_json::Value| -> Option<(PathBuf, PathBuf, String)> {
        let obj = json.as_object()?;

        // Retrieve the target directory.
        let target_dir_str = obj.get("target_directory")?.as_str()?;
        let target_dir_path = Path::new(target_dir_str).to_path_buf();

        // Retrieve the first package as an object.
        let pkgs = obj.get("packages")?.as_array()?;

        // Find the package with the matching manifest.
        let pkg = pkgs.iter().find_map(|pkg| {
            let s = pkg.get("manifest_path")?.as_str()?;
            match s == manifest_path_str {
                true => Some(pkg),
                false => None,
            }
        })?;

        // Search the targets for one containing a dylib output.
        let targets = pkg.get("targets")?.as_array()?;
        let target = targets.iter().find_map(|target| {
            let kind = target.get("kind")?.as_array()?;
            if kind.iter().find(|k| k.as_str() == Some("dylib")).is_some() {
                return Some(target);
            } else {
                None
            }
        })?;

        // Target name and src path.
        let lib_name = target.get("name")?.as_str()?.to_string();
        let src_root_str = target.get("src_path")?.as_str()?;
        let src_root_path = Path::new(src_root_str).to_path_buf();

        Some((target_dir_path, src_root_path, lib_name))
    };

    let (target_dir_path, src_root_path, lib_name) =
        read_json(&json).ok_or(WatchError::NoDylibTarget)?;
    let src_dir_path = src_root_path
        .parent()
        .expect("src root has no parent directory");

    // Begin watching the src path.
    let (tx, event_rx) = mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new_immediate(move |ev| {
        tx.send(ev).ok();
    })?;
    watcher.watch(src_dir_path, notify::RecursiveMode::Recursive)?;

    // Collect the paths.
    let manifest_path = path.to_path_buf();
    let src_path = src_dir_path.to_path_buf();

    // Collect the package info.
    let package_info = PackageInfo {
        manifest_path,
        src_path,
        lib_name,
        target_dir_path,
    };

    Ok(Watch {
        package_info,
        _watcher: watcher,
        event_rx,
    })
}

impl Watch {
    /// The path to the package's `Cargo.toml`.
    pub fn manifest_path(&self) -> &Path {
        &self.package_info.manifest_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.package_info.src_path
    }

    /// Wait for the library to be re-built after some change.
    pub fn next(&self) -> Result<Package, NextError> {
        loop {
            let _event = match self.event_rx.recv() {
                Err(_) => return Err(NextError::ChannelClosed),
                Ok(event) => event,
            };
            return Ok(self.package());
        }
    }

    /// The same as `next`, but returns early if there are no pending events.
    pub fn try_next(&self) -> Result<Option<Package>, NextError> {
        match self.event_rx.try_recv() {
            Ok(_event) => return Ok(Some(self.package())),
            Err(mpsc::TryRecvError::Disconnected) => Err(NextError::ChannelClosed),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
        }
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
    pub fn manifest_path(&self) -> &Path {
        &self.info.manifest_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.info.src_path
    }

    /// Builds the package's dynamic library target.
    pub fn build(&self) -> Result<Build<'a>, BuildError> {
        let PackageInfo {
            ref manifest_path,
            ref lib_name,
            ref target_dir_path,
            ..
        } = self.info;

        // Tell cargo to compile the package.
        let manifest_path_str = format!("{}", manifest_path.display());
        let output = std::process::Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest_path_str)
            .arg("--lib")
            .arg("--release")
            .output()?;

        // Check the exit status.
        if let Some(err) = ExitStatusUnsuccessfulError::from_output(&output) {
            return Err(BuildError::from(err));
        }

        // Time stamp the moment of build completion.
        let timestamp = SystemTime::now();

        Ok(Build {
            timestamp,
            output,
            lib_name,
            target_dir_path,
        })
    }
}

impl<'a> Build<'a> {
    /// The output of the cargo process.
    pub fn cargo_output(&self) -> &std::process::Output {
        &self.output
    }

    /// The moment at which the build was completed.
    pub fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    /// The path to the generated dylib target.
    pub fn dylib_path(&self) -> PathBuf {
        let file_stem = self.file_stem();
        self.target_dir_path
            .join("release")
            .join(file_stem)
            .with_extension(dylib_ext())
    }

    /// The path to the temporary dynamic library clone that will be created upon `load`.
    pub fn tmp_dylib_path(&self) -> PathBuf {
        tmp_dir()
            .join(self.tmp_file_stem())
            .with_extension(dylib_ext())
    }

    /// Copy the library to the platform's temporary directory and load it from there.
    ///
    /// Note that the copied dynamic library will be removed on `Drop`.
    ///
    /// # Safety
    ///
    /// Loading dynamic libraries unfortunately appears to be inherently unsafe. See [this
    /// note](https://docs.rs/libloading/0.7.0/libloading/changelog/r0_7_0/index.html#loading-functions-are-now-unsafe)
    /// in the `libloading` documentation for an explanation.
    pub unsafe fn load(self) -> Result<TempLibrary, LoadError> {
        let dylib_path = self.dylib_path();
        let tmp_path = self.tmp_dylib_path();
        let tmp_dir = tmp_path.parent().expect("temp dylib path has no parent");

        // If the library already exists, load it.
        loop {
            if tmp_path.exists() {
                // This is some voodoo to enable reloading of dylib on mac os
                if cfg!(target_os = "macos") {
                    std::process::Command::new("install_name_tool")
                        .current_dir(tmp_dir)
                        .arg("-id")
                        .arg("''")
                        .arg(
                            tmp_path
                                .file_name()
                                .expect("temp dylib path has no file name"),
                        )
                        .output()
                        .expect("ls command failed to start");
                }

                let lib = libloading::Library::new(&tmp_path)
                    .map(Some)
                    .map_err(|err| LoadError::Library { err })?;
                let path = tmp_path;
                let build_timestamp = self.timestamp;
                let tmp = TempLibrary {
                    build_timestamp,
                    path,
                    lib,
                };
                return Ok(tmp);
            }
            // Copy the dylib to the tmp location.
            std::fs::create_dir_all(tmp_dir).map_err(|err| LoadError::Io { err })?;
            std::fs::copy(&dylib_path, &tmp_path).map_err(|err| LoadError::Io { err })?;
        }
    }

    /// Load the library from it's existing location.
    ///
    /// Note that if you do this, you will have to ensure the returned `Library` is dropped before
    /// attempting to re-build the library.
    pub unsafe fn load_in_place(self) -> Result<libloading::Library, libloading::Error> {
        let dylib_path = self.dylib_path();
        libloading::Library::new(dylib_path)
    }

    // The file stem of the built dynamic library.
    fn file_stem(&self) -> String {
        // TODO: On windows, the generated lib does not contain the "lib" prefix.
        // A proper solution would likely involve retrieving the file stem from cargo itself.
        #[cfg(target_os = "windows")]
        {
            format!("{}", self.lib_name)
        }
        #[cfg(not(target_os = "windows"))]
        {
            format!("lib{}", self.lib_name)
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
        self.lib
            .as_ref()
            .expect("lib should always be `Some` until `Drop`")
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

// The temporary directory used by hotlib.
fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join("hotlib")
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
