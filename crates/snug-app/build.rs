//! 构建脚本：把应用清单、版本信息、exe 图标链进 `snug.exe`。
//!
//! 对应 C# 版的 `src/SnugWin.App/app.manifest` + csproj 里的 `<ApplicationIcon>` /
//! `<Version>`：C# 由 MSBuild 调 `rc.exe` 生成带 `RT_MANIFEST` / `RT_ICON` / `RT_VERSION`
//! 的 `.res`，再交给 link.exe 塞进 exe。这里做同一件事，但不引 build-dependency
//! （离线缓存里不一定有 `embed-resource` / `winres`）：直接 `std::process::Command`
//! 调 Windows SDK 的 `rc.exe`，用 `cargo:rustc-link-arg-bins` 把生成的 `.res` 交给 linker。
//!
//! 几条设计约束：
//! - 版本号只在根 `Cargo.toml` 的 `[workspace.package] version` 里写一次，这里读
//!   `CARGO_PKG_VERSION`，写进 `FILEVERSION` / `PRODUCTVERSION` / 字符串表。
//! - 找不到 `rc.exe` 时只 `cargo:warning`，**不阻塞编译**：没有资源的 exe 依然能跑，
//!   只是不是最终发布形态（exe 图标、清单里的 DPI 感知都会缺）。
//! - 用 `rustc-link-arg-bins` 而不是 `rustc-link-arg`：只给 bin 目标加链接参数，
//!   别把 `requireAdministrator` 清单塞进 `cargo test` 的测试 harness（那会弹 UAC）。
//!
//! 手动验证：`cargo build --release -p snug-app` 之后
//! `mt.exe -inputresource:snug.exe;#1 -out:x.manifest` 抽清单、
//! `(Get-Item snug.exe).VersionInfo` 看版本、资源管理器看图标。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 资源脚本里引用的相对路径（rc.exe 的工作目录是 crate 根目录）。
const ICON_FILE: &str = "assets/snugwin.ico";
const MANIFEST_FILE: &str = "snug-app.manifest";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={MANIFEST_FILE}");
    println!("cargo:rerun-if-changed={ICON_FILE}");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());

    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=未找到 Windows SDK 的 rc.exe（可设 SNUG_RC 指过去），\
             跳过清单/版本信息/图标资源：产物能跑，但不是发布形态"
        );
        return;
    };

    let rc_file = out_dir.join("snug-app.rc");
    let res_file = out_dir.join("snug-app.res");
    if let Err(error) = fs::write(&rc_file, resource_script(&version)) {
        println!("cargo:warning=写资源脚本失败（{error}），跳过清单/版本信息/图标资源");
        return;
    }

    // rc.exe 的 cwd 必须是 crate 根目录，脚本里的相对路径（图标、清单）才解析得到。
    let output = Command::new(&rc)
        .current_dir(&manifest_dir)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output();

    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            println!(
                "cargo:warning=rc.exe 编译资源失败（{}）：{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        Err(error) => {
            println!("cargo:warning=无法运行 rc.exe（{error}），跳过清单/版本信息/图标资源");
            return;
        }
    }

    // 只作用于 bin 目标：测试 harness 不带清单，`cargo test` 不会要管理员权限。
    println!("cargo:rustc-link-arg-bins={}", res_file.display());
}

/// 生成 rc 资源脚本：图标 + 清单 + 版本信息。
///
/// 版本块用 `FILEOS 0x40004`（NT 上的 Win32）、`FILETYPE 0x1`（应用）、
/// 语言 0x0409 代码页 1200（UTF-16），跟 C# 产物一致。
fn resource_script(version: &str) -> String {
    let (major, minor, patch, build) = version_tuple(version);
    format!(
        "// 由 build.rs 生成，别手改。\n\
         #pragma code_page(65001)\n\
         \n\
         1 ICON \"{ICON_FILE}\"\n\
         1 24 \"{MANIFEST_FILE}\"\n\
         \n\
         1 VERSIONINFO\n\
         FILEVERSION {major},{minor},{patch},{build}\n\
         PRODUCTVERSION {major},{minor},{patch},{build}\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS 0x0L\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
         \x20   BLOCK \"StringFileInfo\"\n\
         \x20   BEGIN\n\
         \x20       BLOCK \"040904b0\"\n\
         \x20       BEGIN\n\
         \x20           VALUE \"CompanyName\", \"nicocat\"\n\
         \x20           VALUE \"FileDescription\", \"SnugWin\"\n\
         \x20           VALUE \"FileVersion\", \"{version}\"\n\
         \x20           VALUE \"InternalName\", \"snug\"\n\
         \x20           VALUE \"LegalCopyright\", \"MIT License \u{b7} 2026 nicocat\"\n\
         \x20           VALUE \"OriginalFilename\", \"snug.exe\"\n\
         \x20           VALUE \"ProductName\", \"SnugWin\"\n\
         \x20           VALUE \"ProductVersion\", \"{version}\"\n\
         \x20       END\n\
         \x20   END\n\
         \x20   BLOCK \"VarFileInfo\"\n\
         \x20   BEGIN\n\
         \x20       VALUE \"Translation\", 0x409, 1200\n\
         \x20   END\n\
         END\n"
    )
}

/// `"1.2.0"` → `(1, 2, 0, 0)`。预发布后缀（`1.2.0-beta`）只取数字前缀，缺的段补 0。
fn version_tuple(version: &str) -> (u16, u16, u16, u16) {
    let mut parts = [0u16; 4];
    for (slot, part) in parts.iter_mut().zip(version.split('.')) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        *slot = digits.parse().unwrap_or(0);
    }
    (parts[0], parts[1], parts[2], parts[3])
}

/// 找 rc.exe：先看显式覆盖与 vcvars 的变量，再扫 Windows SDK 安装目录，
/// 版本目录按数字从高到低（`10.0.26100.0` 优先），最后回退到 `bin\<arch>\rc.exe`。
fn find_rc() -> Option<PathBuf> {
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(custom) = env::var_os("SNUG_RC") {
        candidates.push(PathBuf::from(custom));
    }

    // vcvars64.bat 下 WindowsSdkBinPath 形如 ...\bin\10.0.26100.0\x64\
    if let Some(bin) = env::var_os("WindowsSdkBinPath") {
        let bin = PathBuf::from(bin);
        candidates.push(bin.join("rc.exe"));
        candidates.push(bin.join(arch).join("rc.exe"));
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(dir).join("bin"));
    }
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(dir) = env::var_os(key) {
            roots.push(PathBuf::from(dir).join("Windows Kits").join("10").join("bin"));
        }
    }

    for root in roots {
        for version_dir in sdk_versions(&root) {
            candidates.push(version_dir.join(arch).join("rc.exe"));
        }
        candidates.push(root.join(arch).join("rc.exe"));
    }

    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// SDK `bin` 下的版本目录，按版本号降序。非版本号目录（`x64` / `x86`）会被跳过。
fn sdk_versions(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let parts = name.split('.').map(|part| part.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
            Some((parts, path))
        })
        .collect();
    versions.sort_by(|left, right| right.0.cmp(&left.0));
    versions.into_iter().map(|(_, path)| path).collect()
}

// 说明：这里不写 `#[cfg(test)]` 单测——cargo 不会用 `--test` 编译 build script，
// 那些测试永远不会执行（看着绿其实是假绿）。版本号与清单是否真进了 exe，
// 由产物验证兜底：`.VersionInfo` 的 FileVersion/ProductVersion、`mt.exe` 抽清单。

