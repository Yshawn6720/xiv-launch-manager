//! 國際服提權 helper 的工作檔。
//!
//! 一般權限的 parent 以 [`write_job`] 建立 `job.json`，把它的 SHA-256 放在 helper 的命令列；
//! helper 以 [`read_job`] 拒絕共用寫入開啟、核對 SHA-256 後才採信內容。國際服的區塊 hash 取自
//! HTTPS 清單，經命令列 SHA-256 綁定，落地成檔案後不能被替換。job 只含路徑、repo、版本與區塊
//! hash，不含 SID／UID／密碼／註冊 URL；進度檔也不在 job 裡，固定是 [`progress_path`]。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::iter;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};
use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::error::{AppError, AppResult};
use crate::game::MAX_EXPANSION;
use crate::patch::list::{Repo, is_version_id};
use crate::patch::zipatch::is_dos_device;

const JOB_FILE: &str = "job.json";
/// helper 進度檔名，見 [`progress_path`]。
const PROGRESS_FILE: &str = "progress.json";
/// job 大小上限（本專案訂定）：正常的 job 只有數十 KB（每個 patch 一組路徑與區塊 hash）。
const MAX_JOB_BYTES: u64 = 16 * 1024 * 1024;
const SHA256_LEN: usize = 32;

/// job 中的一個已下載 patch；欄位取自 patch list（protocols §5.1）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobPatch {
    /// 已下載的 patch 檔絕對路徑（一般權限 parent 建立）。
    pub path: PathBuf,
    pub repo: Repo,
    pub version_id: String,
    pub length: u64,
    pub hash_block_size: u64,
    pub hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    /// parent 設定中的國際服安裝根目錄；helper 只拿來與 HKLM 解析結果交叉核對，不寫入這裡。
    pub expected_install: PathBuf,
    /// 依清單順序。
    pub patches: Vec<JobPatch>,
}

/// helper 進度檔：固定是 job 目錄（`job.json` 所在目錄）下的 `progress.json`，不由 job 內容決定。
/// parent 預先以 `create_new` 建立空檔，helper 只覆寫內容。
pub fn progress_path(job_dir: &Path) -> PathBuf {
    job_dir.join(PROGRESS_FILE)
}

/// 由 parent 呼叫：在 `dir` 內以 `create_new` 寫 `job.json` 並 `sync_all`。bytes 先在記憶體序列化，
/// SHA-256 由同一份 bytes 計算；回傳（絕對路徑, 小寫 hex SHA-256）。job 內的路徑不合
/// [`read_job`] 的規則時不寫檔。
pub fn write_job(dir: &Path, job: &Job) -> AppResult<(PathBuf, String)> {
    validate(job)?;
    let bytes = serde_json::to_vec(job).map_err(|e| AppError::Io {
        context: "序列化更新工作檔".into(),
        message: e.to_string(),
    })?;
    let sha256 = sha256_hex(&bytes)?;
    let path = std::path::absolute(dir.join(JOB_FILE))
        .map_err(|e| AppError::io("解析更新工作檔路徑", e))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| AppError::io("建立更新工作檔", e))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| AppError::io("寫入更新工作檔", e))?;
    Ok((path, sha256))
}

/// 由 helper 呼叫：以拒絕共用寫入開啟 `path`（讀取期間其他 handle 不能寫入），讀全部 bytes，
/// SHA-256 須等於命令列的 `expected_sha256`，才反序列化並檢查路徑。錯誤不回顯 job 內容。
/// 連同仍拒絕共用寫入的 handle 一起回傳，供 helper 核對它的最終路徑。
pub fn read_job(path: &Path, expected_sha256: &str) -> AppResult<(Job, File)> {
    if !path.is_absolute() {
        return Err(rejected("路徑不是絕對路徑"));
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .map_err(|e| AppError::io("開啟更新工作檔", e))?;
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_JOB_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| AppError::io("讀取更新工作檔", e))?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err(rejected("檔案過大"));
    }
    if sha256_hex(&bytes)? != expected_sha256 {
        return Err(rejected("SHA-256 與命令列不符"));
    }
    // serde 的錯誤訊息可能引用內容，只回報格式不符。
    let job: Job = serde_json::from_slice(&bytes).map_err(|_| rejected("格式不符"))?;
    validate(&job)?;
    Ok((job, file))
}

/// SHA-256（小寫 hex）。crate 依賴只加 `windows` features，故用 Windows CNG 而不另引 sha2。
pub(crate) fn sha256_hex(bytes: &[u8]) -> AppResult<String> {
    // BCryptHash 的長度參數是 u32；windows crate 在超過時會 panic，先擋下。
    if u32::try_from(bytes.len()).is_err() {
        return Err(AppError::Install {
            context: "計算 SHA-256 的資料過大".into(),
        });
    }
    let mut digest = [0u8; SHA256_LEN];
    // SAFETY: BCRYPT_SHA256_ALG_HANDLE 是系統提供的 pseudo-handle（Windows 10 起），不需開啟或關閉；
    // 輸入與輸出都是呼叫期間存活的切片，長度已確認放得進 u32。quality-allow: Win32 FFI，CNG 雜湊沒有安全包裝
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut digest) };
    status.ok().map_err(|e| AppError::Io {
        context: "計算 SHA-256".into(),
        message: e.message(),
    })?;
    Ok(hex::encode(digest))
}

/// 64 位小寫 hex，即 [`sha256_hex`] 的輸出形式（helper 命令列的 job SHA-256）。
pub(crate) fn is_sha256_hex(s: &str) -> bool {
    s.len() == SHA256_LEN * 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 所有路徑必須是 [`is_plain_absolute`]；至少一個 patch；repo 為 game 或 ex1–ex5、versionId 符合
/// protocols §5.1 格式（這兩項不合要到寫 `.ver` 才會被發現，那時遊戲目錄已寫過；versionId 會原樣
/// 寫進 `.ver`）。
fn validate(job: &Job) -> AppResult<()> {
    let paths = iter::once(&job.expected_install).chain(job.patches.iter().map(|p| &p.path));
    for path in paths {
        if !is_plain_absolute(path) {
            return Err(rejected("路徑必須是絕對路徑"));
        }
    }
    if job.patches.is_empty() {
        return Err(rejected("沒有 patch"));
    }
    if !job.patches.iter().all(|p| is_known_repo(p.repo)) {
        return Err(rejected("repo 不是 game 或 ex1–ex5"));
    }
    if !job.patches.iter().all(|p| is_version_id(&p.version_id)) {
        return Err(rejected("versionId 不符 YYYY.MM.DD.NNNN.NNNN"));
    }
    Ok(())
}

/// 一般的絕對路徑：磁碟代號或 UNC 開頭、不含 `.`／`..`，拒絕 `\\?\`、`\\.\` 等裝置命名空間，
/// 任一成分也不得是 DOS 裝置名（否則開到裝置而不是檔案）。
pub(crate) fn is_plain_absolute(path: &Path) -> bool {
    let mut components = path.components();
    let disk_or_unc = matches!(
        components.next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(..))
    );
    disk_or_unc
        && components.next() == Some(Component::RootDir)
        && components.all(|c| match c {
            Component::Normal(part) => !is_dos_device(&part.to_string_lossy()),
            _ => false,
        })
}

fn is_known_repo(repo: Repo) -> bool {
    match repo {
        Repo::Game => true,
        Repo::Expansion(n) => (1..=MAX_EXPANSION).contains(&n),
    }
}

fn rejected(reason: &str) -> AppError {
    AppError::Install {
        context: format!("更新工作檔無效：{reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> Job {
        Job {
            expected_install: dir.join("FINAL FANTASY XIV - A Realm Reborn"),
            patches: vec![
                JobPatch {
                    path: dir.join("patches").join("D2026.08.07.0000.0000.patch"),
                    repo: Repo::Expansion(1),
                    version_id: "2026.08.07.0000.0000".into(),
                    length: 52_848,
                    hash_block_size: 50_000_000,
                    hashes: vec!["625499a52faf29e527543ee969e17ff9a81edcb6".into()],
                },
                JobPatch {
                    path: dir.join("patches").join("H2026.09.14.0000.0000.patch"),
                    repo: Repo::Game,
                    version_id: "2026.09.14.0000.0000".into(),
                    length: 100_000_000,
                    hash_block_size: 50_000_000,
                    hashes: vec!["0".repeat(40), "1".repeat(40)],
                },
            ],
        }
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b"").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let job = sample(dir.path());
        let (path, sha256) = write_job(dir.path(), &job).unwrap();
        assert!(path.is_absolute());
        assert_eq!(path, dir.path().join(JOB_FILE));
        assert!(is_sha256_hex(&sha256));
        assert_eq!(sha256, sha256_hex(&std::fs::read(&path).unwrap()).unwrap());
        assert_eq!(read_job(&path, &sha256).unwrap().0, job);
    }

    #[test]
    fn write_refuses_existing_job() {
        let dir = tempfile::tempdir().unwrap();
        let job = sample(dir.path());
        write_job(dir.path(), &job).unwrap();
        assert!(write_job(dir.path(), &job).is_err());
    }

    #[test]
    fn read_rejects_sha256_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha256) = write_job(dir.path(), &sample(dir.path())).unwrap();
        let other = sha256_hex(b"other").unwrap();
        assert_ne!(other, sha256);
        assert!(read_job(&path, &other).is_err());
        assert!(read_job(&path, &sha256.to_uppercase()).is_err());
    }

    #[test]
    fn read_rejects_job_modified_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha256) = write_job(dir.path(), &sample(dir.path())).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replace("625499a5", "00000000");
        assert_ne!(tampered, text);
        std::fs::write(&path, tampered).unwrap();
        assert!(read_job(&path, &sha256).is_err());
    }

    #[test]
    fn read_fails_while_another_handle_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha256) = write_job(dir.path(), &sample(dir.path())).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(read_job(&path, &sha256).is_err());
        drop(writer);
        assert!(read_job(&path, &sha256).is_ok());
    }

    #[test]
    fn rejects_non_plain_paths() {
        let dir = tempfile::tempdir().unwrap();
        let edited = |edit: &dyn Fn(&mut Job)| {
            let mut job = sample(dir.path());
            edit(&mut job);
            job
        };
        let cases = [
            edited(&|j| j.expected_install = PathBuf::from(r"SquareEnix\FINAL FANTASY XIV")),
            edited(&|j| j.patches[1].path = PathBuf::from(r"\patches\x.patch")),
            edited(&|j| j.patches[0].path = PathBuf::from(r"C:x.patch")),
            edited(&|j| j.patches[0].path = dir.path().join("..").join("x.patch")),
            edited(&|j| j.patches[0].path = PathBuf::from(r"\\.\PhysicalDrive0")),
            edited(&|j| j.expected_install = PathBuf::from(r"\\?\C:\x\FINAL FANTASY XIV")),
            edited(&|j| j.patches[0].path = dir.path().join("COM1")),
            edited(&|j| j.expected_install = dir.path().join("nul.txt").join("x")),
        ];

        for (i, job) in cases.iter().enumerate() {
            let sub = dir.path().join(format!("case{i}"));
            std::fs::create_dir(&sub).unwrap();
            // parent 端就拒絕，不寫檔。
            assert!(write_job(&sub, job).is_err(), "case {i}");
            assert!(!sub.join(JOB_FILE).exists(), "case {i}");
            // 繞過 write_job 自行寫出、SHA-256 正確的 job，helper 端同樣拒絕。
            let bytes = serde_json::to_vec(job).unwrap();
            let path = sub.join(JOB_FILE);
            std::fs::write(&path, &bytes).unwrap();
            assert!(read_job(&path, &sha256_hex(&bytes).unwrap()).is_err(), "case {i}");
        }

        // job 路徑本身也必須是絕對路徑。
        let (_, sha256) = write_job(dir.path(), &sample(dir.path())).unwrap();
        assert!(read_job(Path::new(JOB_FILE), &sha256).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_repo() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(sample(dir.path())).unwrap();
        value["sid"] = serde_json::Value::String("x".into());
        let bytes = serde_json::to_vec(&value).unwrap();
        let path = dir.path().join("unknown.json");
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_job(&path, &sha256_hex(&bytes).unwrap()).is_err());

        // 舊格式的 progressPath（進度檔已固定為 job 目錄下的 progress.json）同樣是未知欄位。
        let mut value = serde_json::to_value(sample(dir.path())).unwrap();
        value["progressPath"] = serde_json::json!(dir.path().join("progress.json"));
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_job(&path, &sha256_hex(&bytes).unwrap()).is_err());

        let mut job = sample(dir.path());
        job.patches[0].repo = Repo::Expansion(6);
        assert!(write_job(dir.path(), &job).is_err());
        job.patches[0].repo = Repo::Expansion(0);
        assert!(write_job(dir.path(), &job).is_err());
        job.patches.clear();
        assert!(write_job(dir.path(), &job).is_err());
    }

    #[test]
    fn rejects_malformed_version_id() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["2026.08.07", "2026.08.07.0000.000a", "2026-08-07-0000-0000", "../../x"] {
            let mut job = sample(dir.path());
            job.patches[1].version_id = bad.into();
            assert!(write_job(dir.path(), &job).is_err(), "{bad}");
            // 繞過 write_job 自行寫出、SHA-256 正確的 job，helper 端同樣拒絕（helper exit 10）。
            let bytes = serde_json::to_vec(&job).unwrap();
            let path = dir.path().join(JOB_FILE);
            std::fs::write(&path, &bytes).unwrap();
            assert!(read_job(&path, &sha256_hex(&bytes).unwrap()).is_err(), "{bad}");
        }
    }
}
