use anyhow::Context;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

// <https://stackoverflow.com/questions/67087597/is-it-possible-to-use-rusts-log-info-for-tests>
#[cfg(test)]
use std::println as debug; // Workaround to use println! for logs.

use crate::InitError;

/// This function is a hack for Android.  Android passes an array of paths, the
/// first of which is `libapp.so` the second of which is a long (virtual) path
/// to where Android *would* extract the libapp.so binary, if that feature is
/// turned on for the APK.  This is all to work around old versions of Android
/// having a broken dlopen where dlopen('libapp.so') would fail. We currently
/// exploit this behavior to use the long path to work backwards to find the apk
/// dir so we can look up the apk split that contains libapp.so This is fragile,
/// and just a bad design. We should instead teach the engine to pass us the
/// path to the apk_dir.
/// This function takes that long virtual path and grabs the part which we think
/// should be the app data dir.
/// e.g. when input:
/// "/data/app/~~7LtReIkm5snW_oXeDoJ5TQ==/com.example.shorebird_test-rpkDZSLBRv2jWcc1gQpwdg==/lib/x86_64/libapp.so"
/// Will return:
/// "/data/app/~~7LtReIkm5snW_oXeDoJ5TQ==/com.example.shorebird_test-rpkDZSLBRv2jWcc1gQpwdg=="
fn app_data_dir_from_libapp_path(libapp_path: &str) -> Result<PathBuf, InitError> {
    let path = PathBuf::from(libapp_path);
    let root = path.ancestors().nth(3).ok_or(InitError::InvalidArgument(
        "original_libapp_paths".to_string(),
        format!("Invalid path: {}", libapp_path),
    ))?;
    Ok(PathBuf::from(root))
}

/// Android splits APKs into multiple files, and we need to find the one that
/// contains the library we want.  However the architecture names for the
/// apk splits is different from the architecture names for the library paths
/// within those split apks.  We need to know both.
struct ArchNames {
    // Name used in the apk split, e.g. base-armeabi_v7a.apk
    apk_split: &'static str,
    // Name used in the library path, e.g. lib/armeabi-v7a/libapp.so
    // Note the - instead of _.
    lib_dir: &'static str,
}

/// Get the APK split names for the current architecture.
fn android_arch_names() -> &'static ArchNames {
    // This was generated by looking at what apk splits are generated by
    // bundletool.
    // https://developer.android.com/ndk/guides/abis
    #[cfg(target_arch = "x86")]
    static ARCH: ArchNames = ArchNames {
        apk_split: "x86",
        lib_dir: "x86",
    };
    #[cfg(target_arch = "x86_64")]
    // x86_64 uses _ for both split and library paths.
    static ARCH: ArchNames = ArchNames {
        apk_split: "x86_64", // e.g. standalone-x86_64_hdpi.apk
        lib_dir: "x86_64",   // e.g. lib/x86_64/libapp.so
    };
    #[cfg(target_arch = "aarch64")]
    // Note the _ in the split name, but the - in the lib dir.
    static ARCH: ArchNames = ArchNames {
        apk_split: "arm64_v8a",
        lib_dir: "arm64-v8a",
    };
    #[cfg(target_arch = "arm")]
    // Note the _ in the split name, but the - in the lib dir.
    static ARCH: ArchNames = ArchNames {
        apk_split: "armeabi_v7a", // e.g. base-armeabi_v7a.apk
        lib_dir: "armeabi-v7a",   // e.g. lib/armeabi-v7a/libapp.so
    };
    &ARCH
}

// This is public so c_api can use this for testing.
pub(crate) fn get_relative_lib_path(lib_name: &str) -> PathBuf {
    PathBuf::from("lib")
        .join(android_arch_names().lib_dir)
        .join(lib_name)
}

// This is just a tuple of the archive and the internal path to the library.
// Ideally we'd just return the ZipFile itself, but I don't know how to set
// up the references correctly, ZipFile contains a borrow into the ZipArchive.
// And I'm not the right Rust to keep a reference to both with proper lifetimes.
#[derive(Debug)]
struct ZipLocation {
    archive: zip::ZipArchive<fs::File>,
    internal_path: String,
}

/// Given a zip file, check if it contains the library we want.
fn check_for_lib_path(zip_path: &Path, lib_path: &str) -> anyhow::Result<ZipLocation> {
    let apk = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    if apk.file_names().any(|name| name == lib_path) {
        return Ok(ZipLocation {
            archive: apk,
            internal_path: lib_path.to_owned(),
        });
    }
    Err(anyhow::anyhow!("Library not found in APK"))
}

/// Given a directory of APKs, find the one that contains the library we want.
/// This has to be done due to split APKs.
fn find_and_open_lib(apks_dir: &Path, lib_name: &str) -> anyhow::Result<ZipLocation> {
    // Read the library out of the APK.  We only really need to do this if it
    // isn't already extracted on disk (which it won't be by default from the
    // play store).

    // First check ones with our arch in the name, in any order.
    let arch = android_arch_names();
    let lib_path = get_relative_lib_path(lib_name)
        .to_str()
        .context("Invalid lib path")?
        .to_owned();

    for entry in fs::read_dir(apks_dir)? {
        let entry = entry?;
        let path = entry.path(); // returns the absolute path.
        if path.is_dir() {
            continue;
        }
        // file_name returns an OsStr which only ever fails to convert to a str
        // on systems that support non-unicode filenames, which is not a problem
        // for us on Android, but we still take extra caution to never crash.
        // This is written as a nested if statement to avoid a double unwrap
        // as well as let coverage see us take all paths (since we'd never
        // take an OsStr fail path).
        if let Some(filename) = path.file_name() {
            if let Some(filename) = filename.to_str() {
                // Note this only examines .apks with the arch in the name
                // so it will not examine the base.apk.
                // We could remove the apk_split check and assume that the
                // first apk to contain the library is the right one?
                if filename.ends_with(".apk") && filename.contains(arch.apk_split) {
                    debug!("Checking APK: {:?}", path);
                    if let Ok(zip) = check_for_lib_path(&path, &lib_path) {
                        debug!("Found lib in apk split: {:?}", path);
                        return Ok(zip);
                    }
                }
            }
        }
    }
    // If we failed to find a split, assume the base.apk contains the library.
    let base_apk_path = apks_dir.join("base.apk");
    debug!("Checking base APK: {:?}", base_apk_path);
    check_for_lib_path(&base_apk_path, &lib_path)
}

/// Given a directory of APKs, find the one that contains the library we want.
/// This has to be done due to split APKs.
/// This is public so c_api can use this for testing.
pub(crate) fn open_base_lib(apks_dir: &Path, lib_name: &str) -> anyhow::Result<Cursor<Vec<u8>>> {
    // As far as I can tell, Android provides no apis for reading per-platform
    // assets (e.g. libapp.so) from an APK.  Both Facebook and Chromium
    // seem to have written their own code to do this:
    // https://github.com/facebook/SoLoader/blob/main/java/com/facebook/soloader/DirectApkSoSource.java
    // https://chromium.googlesource.com/chromium/src/base/+/a5ca5def0453df367b9c42e9817a33d2a21e75fe/android/java/src/org/chromium/base/library_loader/Linker.java
    // Previously I tried reading libapp.so from from the AssetManager, but
    // it does show the lib/ directory in the list of assets.
    // https://github.com/shorebirdtech/updater/pull/6

    // Ideally we would do this apk reading from the C++ side and keep the rust
    // portable, but we have a zip library here, and don't on the C++ side.

    let mut zip_location = find_and_open_lib(apks_dir, lib_name)?;
    let mut zip_file = zip_location
        .archive
        .by_name(&zip_location.internal_path)
        .context("Failed to find libapp.so in APK")?;

    // Cursor (rather than ZipFile) is only necessary because bipatch expects
    // Seek + Read for the input file.  I don't think it actually needs to
    // seek backwards, so Read is probably sufficient.  If we made bipatch
    // only depend on Read we could avoid loading the library fully into memory.
    let mut buffer = Vec::new();
    zip_file.read_to_end(&mut buffer)?;
    Ok(Cursor::new(buffer))
}

pub fn libapp_path_from_settings(original_libapp_paths: &[String]) -> Result<PathBuf, InitError> {
    // FIXME: This makes the assumption that the last path provided is the full
    // path to the libapp.so file.  This is true for the current engine, but
    // may not be true in the future.  Better would be for the engine to
    // pass us the path to the base.apk.
    // https://github.com/shorebirdtech/shorebird/issues/283
    // This is where the paths are set today:
    // First path is "libapp.so" (for dlopen), second is a full path:
    // https://github.com/flutter/engine/blob/a7c9cc58a71c5850be0215ab1997db92cc5e8d3e/shell/platform/android/io/flutter/embedding/engine/loader/FlutterLoader.java#L264
    // Which is composed from nativeLibraryDir:
    // https://developer.android.com/reference/android/content/pm/ApplicationInfo#nativeLibraryDir
    let full_libapp_path = original_libapp_paths
        .last()
        .ok_or(InitError::InvalidArgument(
            "original_libapp_paths".to_string(),
            "empty".to_string(),
        ))?;
    // We could probably use sourceDir instead?
    // https://developer.android.com/reference/android/content/pm/ApplicationInfo#sourceDir
    // and splitSourceDirs (api 21+)
    // https://developer.android.com/reference/android/content/pm/ApplicationInfo#splitSourceDirs
    debug!("Finding apk from: {:?}", full_libapp_path);
    app_data_dir_from_libapp_path(full_libapp_path)
}

// These are mostly stub tests to prevent warnings about unused fields.
#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;
    use tempdir::TempDir;
    use zip::write::FileOptions;
    use zip::ZipWriter;

    // Takes a path to the zip to create as well as a list of file names to
    // create in the zip.  The files will be empty.
    fn create_zip_with_empty_files(zip_path: &Path, files: Vec<&str>) {
        let file = File::create(zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        for file in files {
            zip.start_file(file, FileOptions::default()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn find_and_open_lib_test() {
        let tmp_dir = TempDir::new("example").unwrap();
        let error = super::find_and_open_lib(tmp_dir.path(), "libapp.so").unwrap_err();
        assert!(error.to_string().contains("No such file or directory"));

        // Write an empty file (invalid apk) to the base apk.
        let base_apk_path = tmp_dir.path().join("base.apk");
        std::fs::File::create(base_apk_path).unwrap();
        let error = super::find_and_open_lib(tmp_dir.path(), "libapp.so").unwrap_err();
        assert_eq!(error.to_string(), "invalid Zip archive: Invalid zip header");

        // Write an empty zip as the base.apk.
        let base_apk_path = tmp_dir.path().join("base.apk");
        create_zip_with_empty_files(&base_apk_path, vec![]);

        let error = super::find_and_open_lib(tmp_dir.path(), "libapp.so").unwrap_err();
        assert_eq!(error.to_string(), "Library not found in APK");
    }

    #[test]
    fn find_and_open_lib_base_apk() {
        // Create a valid apk (zip) with an empty libapp.so with the right path.
        let tmp_dir = TempDir::new("example").unwrap();

        let base_apk_path = tmp_dir.path().join("base.apk");
        let arch = super::android_arch_names();
        let lib_path = format!("lib/{}/libapp.so", arch.lib_dir);
        create_zip_with_empty_files(&base_apk_path, vec![&lib_path]);

        let zip_location = super::find_and_open_lib(tmp_dir.path(), "libapp.so").unwrap();
        // Success!
        assert_eq!(zip_location.internal_path, lib_path);
        // Otherwise coverage complains that we haven't used the Debug trait
        // even though the Debug trait is required for assert_eq!.
        let debug_str = format!("{:?}", zip_location);
        assert!(debug_str.contains("ZipLocation"));
    }

    #[test]
    fn find_and_open_lib_split_apk() {
        // Create a valid apk (zip) with an empty libapp.so with the right path
        // and a base apk with the wrong path.
        let tmp_dir = TempDir::new("example").unwrap();

        // Write a base.apk with the wrong arch.
        let base_apk_path = tmp_dir.path().join("base.apk");
        create_zip_with_empty_files(&base_apk_path, vec!["lib/wrong/libapp.so"]);

        // Write a split apk with the right arch.
        let arch = super::android_arch_names();
        let split_apk_name = format!("app-hdpi{}-release.apk", arch.apk_split);
        let split_apk_path: std::path::PathBuf = tmp_dir.path().join(split_apk_name);
        let lib_path = format!("lib/{}/libapp.so", arch.lib_dir);
        create_zip_with_empty_files(&split_apk_path, vec![&lib_path]);

        // Write another apk early in the alphabet we skip over since it isn't
        // a split apk.
        let split_apk_path: std::path::PathBuf = tmp_dir.path().join("aaa.apk");
        create_zip_with_empty_files(&split_apk_path, vec![&lib_path]);

        // Write an apk with our arch name but not our library.
        let split_apk_name = format!("aaa{}.apk", arch.apk_split);
        let split_apk_path: std::path::PathBuf = tmp_dir.path().join(split_apk_name);
        create_zip_with_empty_files(&split_apk_path, vec![]);

        let zip_location = super::find_and_open_lib(tmp_dir.path(), "libapp.so").unwrap();
        // Success!
        assert_eq!(zip_location.internal_path, lib_path);
    }

    #[test]
    fn app_data_dir_from_libapp_path_test() {
        let path = "/data/app/~~7LtReIkm5snW_oXeDoJ5TQ==/com.example.shorebird_test-rpkDZSLBRv2jWcc1gQpwdg==/lib/x86_64/libapp.so";
        let dir = super::app_data_dir_from_libapp_path(path).unwrap();
        assert_eq!(
            dir,
            std::path::PathBuf::from("/data/app/~~7LtReIkm5snW_oXeDoJ5TQ==/com.example.shorebird_test-rpkDZSLBRv2jWcc1gQpwdg==")
        );
    }

    #[test]
    fn open_base_lib_test() {
        let tmp_dir = TempDir::new("example").unwrap();
        let error = super::open_base_lib(tmp_dir.path(), "libapp.so").unwrap_err();
        assert!(error.to_string().contains("No such file or directory"));
    }
}
