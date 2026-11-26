use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    slint_build::compile(".").expect("Slint build failed");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=RC_PATH");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR missing"));

    let icon_path = manifest_dir.join("assets").join("app-icon.ico");
    assert!(
        icon_path.is_file(),
        "Windows application icon not found at {}",
        icon_path.display()
    );

    let icon_rc = icon_path.to_string_lossy().replace('\\', "/");
    let major = env::var("CARGO_PKG_VERSION_MAJOR")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    let minor = env::var("CARGO_PKG_VERSION_MINOR")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(1);
    let patch = env::var("CARGO_PKG_VERSION_PATCH")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".to_string());

    let rc_content = format!(
        "1 ICON \"{icon_rc}\"\n\
         \n\
         1 VERSIONINFO\n\
         FILEVERSION {major},{minor},{patch},0\n\
         PRODUCTVERSION {major},{minor},{patch},0\n\
         FILEOS 0x40004\n\
         FILETYPE 0x1\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904b0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"FileDescription\", \"MusicTag Native\"\n\
         \x20     VALUE \"FileVersion\", \"{pkg_version}\"\n\
         \x20     VALUE \"InternalName\", \"musictag.exe\"\n\
         \x20     VALUE \"OriginalFilename\", \"musictag.exe\"\n\
         \x20     VALUE \"ProductName\", \"MusicTag Native\"\n\
         \x20     VALUE \"ProductVersion\", \"{pkg_version}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x0409, 1200\n\
         \x20 END\n\
         END\n"
    );

    let rc_path = out_dir.join("musictag.rc");
    fs::write(&rc_path, rc_content)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", rc_path.display()));

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    match target_env.as_str() {
        "gnu" => compile_gnu_rc(&rc_path, &out_dir, &target_arch),
        "msvc" => compile_msvc_rc(&rc_path, &out_dir),
        other => panic!("unsupported Windows CARGO_CFG_TARGET_ENV: {other:?}"),
    }
}

fn compile_gnu_rc(rc_path: &Path, out_dir: &Path, target_arch: &str) {
    let mut candidates = Vec::new();
    if let Ok(custom) = env::var("WINDRES") {
        if !custom.trim().is_empty() {
            candidates.push(custom);
        }
    }
    match target_arch {
        "x86_64" => candidates.push("x86_64-w64-mingw32-windres".to_string()),
        "x86" => candidates.push("i686-w64-mingw32-windres".to_string()),
        "aarch64" => candidates.push("aarch64-w64-mingw32-windres".to_string()),
        _ => {}
    }
    candidates.push("llvm-windres".to_string());
    candidates.push("windres".to_string());

    let tool = find_working_tool(&candidates, "--version").unwrap_or_else(|| {
        panic!(
            "no Windows GNU resource compiler found (tried: {}). Install mingw-w64 windres or llvm-windres, or set WINDRES.",
            candidates.join(", ")
        )
    });

    let obj_path = out_dir.join("musictag_res.o");
    let mut cmd = Command::new(&tool);
    match target_arch {
        "x86_64" => {
            cmd.arg("--target=pe-x86-64");
        }
        "x86" => {
            cmd.arg("--target=pe-i386");
        }
        _ => {}
    }
    cmd.args(["-O", "coff", "-i"])
        .arg(rc_path)
        .arg("-o")
        .arg(&obj_path);

    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke {tool}: {e}"));
    assert!(
        status.success(),
        "{tool} failed with status {status} while compiling {}",
        rc_path.display()
    );

    println!("cargo:rustc-link-arg-bin=musictag={}", obj_path.display());
}

fn compile_msvc_rc(rc_path: &Path, out_dir: &Path) {
    let mut candidates = Vec::new();
    if let Ok(custom) = env::var("RC_PATH") {
        if !custom.trim().is_empty() {
            candidates.push(custom);
        }
    }
    candidates.push("rc.exe".to_string());
    candidates.push("rc".to_string());
    candidates.push("llvm-rc".to_string());

    let tool = find_working_tool(&candidates, "/?").unwrap_or_else(|| {
        panic!(
            "no Windows MSVC resource compiler found (tried: {}). Install Windows SDK rc.exe or llvm-rc, or set RC_PATH.",
            candidates.join(", ")
        )
    });

    let res_path = out_dir.join("musictag_res.res");
    let status = Command::new(&tool)
        .arg("/fo")
        .arg(&res_path)
        .arg(rc_path)
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke {tool}: {e}"));
    assert!(
        status.success(),
        "{tool} failed with status {status} while compiling {}",
        rc_path.display()
    );

    println!("cargo:rustc-link-arg-bin=musictag={}", res_path.display());
}

fn find_working_tool(candidates: &[String], probe_arg: &str) -> Option<String> {
    for cand in candidates {
        if Command::new(cand).arg(probe_arg).output().is_ok() {
            return Some(cand.clone());
        }
    }
    None
}
