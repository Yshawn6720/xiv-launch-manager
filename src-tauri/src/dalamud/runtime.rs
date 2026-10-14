//! .NET runtime（`runtime/<version>/`）。兩來源 layout 相同：`host/fxr/{v}/hostfxr.dll`、
//! `shared/Microsoft.NETCore.App/{v}/`、`shared/Microsoft.WindowsDesktop.App/{v}/`，所以同版本共用。
//!
//! 完成條件＝提交點：`runtime/<version>/` 只由 [`Staging::commit`] 產生，存在即完整；
//! 不以「三個子目錄存在」判斷（中斷的安裝只會留在 staging）。

use std::fs;
use std::path::{Path, PathBuf};

use super::install::{self, CancelToken, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::store::AppPaths;

const KAMORI_RUNTIME_BASE: &str = "https://kamori.goats.dev/Dalamud/Release/Runtime";
const NUGET_BASE: &str = "https://api.nuget.org/v3-flatcontainer";
const NETCORE_PACKAGE: &str = "microsoft.netcore.app.runtime.win-x64";
const DESKTOP_PACKAGE: &str = "microsoft.windowsdesktop.app.runtime.win-x64";
const NETCORE_FRAMEWORK: &str = "Microsoft.NETCore.App";
const DESKTOP_FRAMEWORK: &str = "Microsoft.WindowsDesktop.App";
/// kamori 的 hash 表含根目錄 `version` 檔（內容＝版本字串，XL 下載完才寫入）。
const VERSION_FILE: &str = "version";

/// goatcorp 來源（protocols §1.5）：DotNet 與 WindowsDesktop 兩個 zip 解到同一目錄，
/// 以 `Runtime/Hashes/{v}` 逐檔 MD5 核對後提交。
pub async fn ensure_goatcorp(
    client: &reqwest::Client,
    paths: &AppPaths,
    version: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PathBuf> {
    let target = paths.runtime_dir(version);
    if target.is_dir() {
        return Ok(target);
    }

    let hashes = cancel
        .race(install::fetch_bytes(
            client.get(format!("{KAMORI_RUNTIME_BASE}/Hashes/{version}")),
            "下載 runtime hash 表",
        ))
        .await?;
    let table = install::parse_hash_table(&hashes, "runtime hash 表")?;

    let staging = Staging::create(paths, "runtime")?;
    let dotnet = staging.path().join("dotnet.zip");
    let desktop = staging.path().join("desktop.zip");
    install::download_to_file(
        client,
        &format!("{KAMORI_RUNTIME_BASE}/DotNet/{version}"),
        &dotnet,
        "下載 .NET runtime",
        progress,
        cancel,
    )
    .await?;
    install::download_to_file(
        client,
        &format!("{KAMORI_RUNTIME_BASE}/WindowsDesktop/{version}"),
        &desktop,
        "下載 Windows Desktop runtime",
        progress,
        cancel,
    )
    .await?;

    install::report(progress, "解壓並核對 .NET runtime", 0, None);
    let content = staging.path().join("content");
    let paths_owned = paths.clone();
    let target_owned = target.clone();
    let version_owned = version.to_string();
    let cancel = cancel.clone();
    install::run_blocking("安裝 .NET runtime", move || {
        cancel.check()?;
        install::extract_zip(&dotnet, &content)?;
        cancel.check()?;
        install::extract_zip(&desktop, &content)?;
        cancel.check()?;
        if install::table_lists(&table, VERSION_FILE) {
            fs::write(content.join(VERSION_FILE), &version_owned)
                .map_err(|e| AppError::io("寫入 runtime version", e))?;
        }
        install::verify_hash_table(&content, &table)?;
        cancel.check()?;
        staging.commit(&paths_owned, &content, &target_owned)
    })
    .await?;
    Ok(target)
}

/// NuGet 來源（protocols §2.5）：兩個 runtime pack `.nupkg` 依 layout 抽出。
/// NuGet 沒有 `.nupkg.sha512`，完整性靠 TLS 與 zip CRC；提交前只確認關鍵檔案到位。
pub async fn ensure_nuget(
    client: &reqwest::Client,
    paths: &AppPaths,
    version: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PathBuf> {
    let target = paths.runtime_dir(version);
    if target.is_dir() {
        return Ok(target);
    }
    let lib_dir = format!("net{}", major_minor(version)?);

    let staging = Staging::create(paths, "runtime")?;
    let core = staging.path().join("netcore.nupkg");
    let desktop = staging.path().join("desktop.nupkg");
    install::download_to_file(
        client,
        &nupkg_url(NETCORE_PACKAGE, version),
        &core,
        "下載 .NET runtime",
        progress,
        cancel,
    )
    .await?;
    install::download_to_file(
        client,
        &nupkg_url(DESKTOP_PACKAGE, version),
        &desktop,
        "下載 Windows Desktop runtime",
        progress,
        cancel,
    )
    .await?;

    install::report(progress, "解壓 .NET runtime", 0, None);
    let content = staging.path().join("content");
    let paths_owned = paths.clone();
    let target_owned = target.clone();
    let version_owned = version.to_string();
    let cancel = cancel.clone();
    install::run_blocking("安裝 .NET runtime", move || {
        cancel.check()?;
        extract_runtime_pack(&core, &content, NETCORE_FRAMEWORK, &version_owned, &lib_dir)?;
        cancel.check()?;
        extract_runtime_pack(&desktop, &content, DESKTOP_FRAMEWORK, &version_owned, &lib_dir)?;
        // 上游改套件結構時會解出空殼；提交後存在即視為完整，所以必須在提交前擋下。
        let v = version_owned.as_str();
        let required = [
            format!("host/fxr/{v}/hostfxr.dll"),
            format!("shared/{NETCORE_FRAMEWORK}/{v}/System.Private.CoreLib.dll"),
            format!("shared/{DESKTOP_FRAMEWORK}/{v}/PresentationCore.dll"),
        ];
        install::verify_files(
            &content,
            required.iter().map(|f| (f.as_str(), None)),
            install::HashAlgo::Md5,
        )?;
        cancel.check()?;
        staging.commit(&paths_owned, &content, &target_owned)
    })
    .await?;
    Ok(target)
}

/// NuGet flat container 的 id 與版本都要小寫。
fn nupkg_url(package: &str, version: &str) -> String {
    let v = version.to_ascii_lowercase();
    format!("{NUGET_BASE}/{package}/{v}/{package}.{v}.nupkg")
}

/// `9.0.11` → `9.0`（runtime pack 內 `lib/net9.0/`）。
fn major_minor(version: &str) -> AppResult<String> {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) if !major.is_empty() && !minor.is_empty() => {
            Ok(format!("{major}.{minor}"))
        }
        _ => Err(AppError::Install {
            context: format!(".NET runtime 版本格式不符：{version}"),
        }),
    }
}

/// `runtimes/win-x64/native/*` 與 `runtimes/win-x64/lib/net{x.y}/*` → `shared/{framework}/{v}/`
/// （保留子目錄，WindowsDesktop 的語系資源在 `lib/net{x.y}/{culture}/`），
/// `hostfxr.dll` → `host/fxr/{v}/`。其他 entry（nuspec、簽章等）略過。
fn extract_runtime_pack(
    nupkg: &Path,
    root: &Path,
    framework: &str,
    version: &str,
    lib_dir: &str,
) -> AppResult<()> {
    let native_prefix = Path::new("runtimes").join("win-x64").join("native");
    let lib_prefix = Path::new("runtimes").join("win-x64").join("lib").join(lib_dir);
    let shared = root.join("shared").join(framework).join(version);
    let fxr = root.join("host").join("fxr").join(version);
    install::extract_zip_mapped(nupkg, |rel| {
        if let Ok(rest) = rel.strip_prefix(&native_prefix) {
            return Some(if rest == Path::new("hostfxr.dll") {
                fxr.join(rest)
            } else {
                shared.join(rest)
            });
        }
        rel.strip_prefix(&lib_prefix).ok().map(|rest| shared.join(rest))
    })
}
