//! 依清單順序套用已下載的 patch（主 note「流程／套用」）；台服程序內套用與國際服提權 helper 共用。
//!
//! 每個 patch：以拒絕共用寫入開啟（保持到該 patch 寫完 `.ver`）→ 區塊 SHA1 核對 → 回檔頭 →
//! ZiPatch 掃描 → 再查一次遊戲是否執行 → 套用 → 寫 `.ver`。核對、掃描、套用都用同一個 handle，
//! 不依路徑重開；成功後不刪 patch 檔（由 parent 刪）。

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::error::{AppError, AppResult};
use crate::patch::job::JobPatch;
use crate::patch::zipatch::{self, ScannedPatch};
use crate::patch::{list, running};

/// 套用失敗的分類，決定呼叫端是否保留 dirty 標記。
#[derive(Debug)]
pub enum ApplyFailure {
    /// 失敗的這個 patch 尚未改動遊戲目錄（核對不符、掃描拒絕、偵測到遊戲執行、取消，或第一筆寫入前開檔
    /// 遇共用違規），之前的 patch 都已完整套用並寫好 `.ver`：遊戲目錄處於某個完整版本，呼叫端可清除
    /// dirty 標記。多個 patch 時不代表遊戲目錄完全未變。
    BeforeWrite(AppError),
    /// 已開始寫入遊戲目錄（套用或寫 `.ver` 失敗，含已寫入後的共用違規）——保留 dirty 標記。
    AfterWrite(AppError),
}

/// 套用進度（僅供顯示）。`done`／`total` 為目前這個 patch 已處理／總共的 patch bytes。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyProgress {
    pub patch_index: usize,
    pub patch_count: usize,
    pub done: u64,
    pub total: u64,
}

/// 依序套用 `patches` 到 `install`（安裝根目錄；套用根目錄一律 `{install}\game`）。`cancelled`
/// 只在 patch 與 patch 之間檢查（套用中不中斷）。版本逐 patch 推進：每個 patch 套用成功後才寫它的
/// `.ver`，失敗即停止，之後的 patch 不碰。
pub fn apply_patches(
    install: &Path,
    patches: &[JobPatch],
    progress: &mut dyn FnMut(ApplyProgress),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ApplyFailure> {
    let game_root = install.join("game");
    let patch_count = patches.len();
    for (patch_index, patch) in patches.iter().enumerate() {
        if cancelled() {
            return Err(ApplyFailure::BeforeWrite(AppError::Cancelled));
        }
        let mut file = open_patch(&patch.path).map_err(ApplyFailure::BeforeWrite)?;
        let scanned =
            prepare(&mut file, patch, install, &game_root).map_err(ApplyFailure::BeforeWrite)?;
        let total = scanned.total_bytes();
        let mut report = |done| {
            progress(ApplyProgress {
                patch_index,
                patch_count,
                done,
                total,
            })
        };
        report(0);
        scanned.apply(&mut report)?;
        zipatch::write_version(&game_root, patch.repo, &patch.version_id)
            .map_err(ApplyFailure::AfterWrite)?;
    }
    Ok(())
}

/// 拒絕共用寫入（也不允許刪除或改名），核對到套用完之間其他 handle 換不掉內容。
fn open_patch(path: &Path) -> AppResult<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .map_err(|e| AppError::io("開啟 patch 檔", e))
}

/// 寫入前的所有檢查；任何失敗都還沒寫入遊戲目錄。
fn prepare<'a>(
    file: &'a mut File,
    patch: &JobPatch,
    install: &Path,
    game_root: &Path,
) -> AppResult<ScannedPatch<'a, File>> {
    list::verify_blocks(&mut *file, patch.length, patch.hash_block_size, &patch.hashes)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| AppError::io("讀取 patch 檔", e))?;
    let scanned = zipatch::scan_patch(file, game_root)?;
    if running::install_in_use(install)? {
        return Err(AppError::GameRunning);
    }
    Ok(scanned)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::patch::list::Repo;
    use crate::patch::running::test_support::{FakeProcess, GAME_PROCESSES};

    /// 台服 ex1 2026.08.07 的實際 patch（52,848 bytes），清單中的長度、區塊大小與 SHA1。
    const TW_EX1_PATCH: &[u8] = include_bytes!("testdata/D2026.08.07.0000.0000.patch");
    const EX1_VERSION: &str = "2026.08.07.0000.0000";
    const EX1_HASH: &str = "625499a52faf29e527543ee969e17ff9a81edcb6";
    const DAT: &str = r"game\sqpack\ex1\030100.win32.dat0";
    const EX1_VER: &str = r"game\sqpack\ex1\ex1.ver";

    /// 假安裝（`game\sqpack\ex1` 預建）與放 patch 檔的獨立目錄（不在安裝內，快照不含它）。
    struct Fixture {
        install: tempfile::TempDir,
        patches: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let install = tempfile::tempdir().expect("tempdir");
            fs::create_dir_all(install.path().join(r"game\sqpack\ex1")).expect("mkdir");
            let patches = tempfile::tempdir().expect("tempdir");
            fs::write(patches.path().join("ex1.patch"), TW_EX1_PATCH).expect("patch");
            Self { install, patches }
        }

        fn patch(&self) -> JobPatch {
            JobPatch {
                path: self.patches.path().join("ex1.patch"),
                repo: Repo::Expansion(1),
                version_id: EX1_VERSION.into(),
                length: TW_EX1_PATCH.len() as u64,
                hash_block_size: 50_000_000,
                hashes: vec![EX1_HASH.into()],
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.install.path().join(rel)
        }

        fn run(&self, patches: &[JobPatch]) -> (Result<(), ApplyFailure>, Vec<ApplyProgress>) {
            self.run_with(patches, &|| false)
        }

        fn run_with(
            &self,
            patches: &[JobPatch],
            cancelled: &dyn Fn() -> bool,
        ) -> (Result<(), ApplyFailure>, Vec<ApplyProgress>) {
            let mut seen = Vec::new();
            let result = apply_patches(self.install.path(), patches, &mut |p| seen.push(p), cancelled);
            (result, seen)
        }

        /// 安裝目錄內所有項目與檔案內容。
        fn snapshot(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
            fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
                for entry in fs::read_dir(dir).expect("read_dir") {
                    let path = entry.expect("entry").path();
                    let rel = path.strip_prefix(base).expect("prefix").to_path_buf();
                    if path.is_dir() {
                        out.push((rel, None));
                        walk(base, &path, out);
                    } else {
                        out.push((rel, Some(fs::read(path).expect("read"))));
                    }
                }
            }
            let mut out = Vec::new();
            walk(self.install.path(), self.install.path(), &mut out);
            out.sort();
            out
        }
    }

    #[test]
    fn applies_real_patch_and_writes_version() {
        let _processes = GAME_PROCESSES.blocking_read();
        let fx = Fixture::new();
        let (result, seen) = fx.run(&[fx.patch()]);
        result.expect("apply");
        assert_eq!(fs::read_to_string(fx.path(EX1_VER)).expect("ver"), EX1_VERSION);
        assert!(fx.path(DAT).is_file());
        let total = TW_EX1_PATCH.len() as u64;
        assert_eq!(seen.first().map(|p| p.done), Some(0));
        assert_eq!(
            seen.last().copied(),
            Some(ApplyProgress {
                patch_index: 0,
                patch_count: 1,
                done: total,
                total,
            })
        );
        // patch 檔由 parent 刪，套用不動它。
        assert_eq!(fs::read(fx.patch().path).expect("patch"), TW_EX1_PATCH);
    }

    #[test]
    fn hash_mismatch_is_before_write_and_install_untouched() {
        let fx = Fixture::new();
        let before = fx.snapshot();
        let mut patch = fx.patch();
        patch.hashes = vec!["0".repeat(40)];
        let (result, seen) = fx.run(&[patch]);
        assert!(matches!(
            result,
            Err(ApplyFailure::BeforeWrite(AppError::PatchVerifyFailed { .. }))
        ));
        assert!(seen.is_empty());
        assert_eq!(fx.snapshot(), before);
    }

    #[test]
    fn patch_held_open_for_writing_is_before_write() {
        let fx = Fixture::new();
        let before = fx.snapshot();
        let writer = OpenOptions::new()
            .write(true)
            .open(fx.patch().path)
            .expect("open writer");
        let (result, _) = fx.run(&[fx.patch()]);
        assert!(matches!(result, Err(ApplyFailure::BeforeWrite(AppError::Io { .. }))));
        assert_eq!(fx.snapshot(), before);
        drop(writer);
    }

    #[test]
    fn cancel_before_first_patch_is_before_write() {
        let fx = Fixture::new();
        let before = fx.snapshot();
        let (result, _) = fx.run_with(&[fx.patch()], &|| true);
        assert!(matches!(result, Err(ApplyFailure::BeforeWrite(AppError::Cancelled))));
        assert_eq!(fx.snapshot(), before);
    }

    #[test]
    fn game_running_is_before_write() {
        let _processes = GAME_PROCESSES.blocking_write();
        let fx = Fixture::new();
        let _game = FakeProcess::start(&fx.path(r"game\ffxiv_dx11.exe"));
        let before = fx.snapshot();
        let (result, _) = fx.run(&[fx.patch()]);
        assert!(matches!(result, Err(ApplyFailure::BeforeWrite(AppError::GameRunning))));
        assert_eq!(fx.snapshot(), before);
    }

    #[test]
    fn later_patch_failure_keeps_earlier_version() {
        let _processes = GAME_PROCESSES.blocking_read();
        let fx = Fixture::new();
        let mut bad = fx.patch();
        bad.version_id = "2026.08.10.0000.0000".into();
        bad.hashes = vec!["0".repeat(40)];
        let (result, _) = fx.run(&[fx.patch(), bad]);
        assert!(matches!(result, Err(ApplyFailure::BeforeWrite(_))));
        assert_eq!(fs::read_to_string(fx.path(EX1_VER)).expect("ver"), EX1_VERSION);
    }

    #[test]
    fn version_write_failure_is_after_write() {
        let _processes = GAME_PROCESSES.blocking_read();
        let fx = Fixture::new();
        // `.ver` 位置被非空目錄佔住：套用成功、rename 取代失敗。
        fs::create_dir(fx.path(EX1_VER)).expect("mkdir");
        fs::write(fx.path(EX1_VER).join("keep"), b"").expect("seed");
        let (result, _) = fx.run(&[fx.patch()]);
        assert!(matches!(result, Err(ApplyFailure::AfterWrite(_))));
        assert!(fx.path(DAT).is_file());
    }

    /// 遊戲以讀取共用開著要寫的 dat：第一筆寫入前開檔即共用違規，視同遊戲執行中，遊戲目錄不變（呼叫端可清
    /// dirty 標記）。已寫入後的共用違規見 zipatch 測試 `file_opened_by_another_handle_mid_apply_is_after_write`。
    #[test]
    fn game_file_held_open_before_first_write_is_before_write() {
        let _processes = GAME_PROCESSES.blocking_read();
        let fx = Fixture::new();
        fs::write(fx.path(DAT), b"").expect("seed dat");
        let before = fx.snapshot();
        let holder = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(fx.path(DAT))
            .expect("hold dat");
        let (result, _) = fx.run(&[fx.patch()]);
        drop(holder);
        assert!(
            matches!(result, Err(ApplyFailure::BeforeWrite(AppError::GameRunning))),
            "{result:?}"
        );
        assert_eq!(fx.snapshot(), before);
    }

    /// 實際 patch 改 SQPK/A（偏移 484）的一個資料 byte 並重算該 chunk 的 CRC32：仍是格式正確的
    /// ZiPatch，只有清單 hash 能分辨。
    fn well_formed_replacement() -> Vec<u8> {
        const CHUNK: usize = 484;
        let mut bytes = TW_EX1_PATCH.to_vec();
        assert_eq!(&bytes[CHUNK + 4..CHUNK + 8], b"SQPK");
        let size = u32::from_be_bytes(bytes[CHUNK..CHUNK + 4].try_into().expect("size")) as usize;
        let body = CHUNK + 8;
        // 子命令 A 的資料從 520 起（zipatch 測試 real_ex1_patch_applies_inside_game_root）。
        bytes[520] ^= 0xff;
        let crc = crc32fast::hash(&bytes[CHUNK + 4..body + size]);
        bytes[body + size..body + size + 4].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// 驗收「核對後替換 patch bytes 被拒，遊戲目錄不變」：parent 核對過後檔案被換成另一份格式正確的
    /// ZiPatch，套用端以 job 的 hash 重新核對，寫入前拒絕。
    #[test]
    fn patch_replaced_after_parent_verify_is_rejected_before_write() {
        let fx = Fixture::new();
        let replaced = well_formed_replacement();
        zipatch::scan_patch(&mut std::io::Cursor::new(&replaced), &fx.path("game"))
            .expect("替換後仍是格式正確的 ZiPatch");
        fs::write(fx.patch().path, &replaced).expect("replace");
        let before = fx.snapshot();
        let (result, seen) = fx.run(&[fx.patch()]);
        assert!(matches!(
            result,
            Err(ApplyFailure::BeforeWrite(AppError::PatchVerifyFailed { .. }))
        ));
        assert!(seen.is_empty());
        assert_eq!(fx.snapshot(), before);
    }

    /// 驗收同上（核對與套用之間）：套用期間 patch 以拒絕共用寫入的 handle 開著，其他 handle 寫不進、改不了名、
    /// 刪不掉，套用的就是核對過的內容。
    #[test]
    fn patch_cannot_be_replaced_while_being_applied() {
        let _processes = GAME_PROCESSES.blocking_read();
        let fx = Fixture::new();
        let path = fx.patch().path;
        let mut attempts = Vec::new();
        let result = apply_patches(
            fx.install.path(),
            &[fx.patch()],
            &mut |p| {
                // 第一筆進度在核對與掃描之後、第一筆寫入之前。
                if p.done == 0 {
                    attempts.push([
                        OpenOptions::new().write(true).open(&path).is_err(),
                        fs::rename(&path, path.with_extension("old")).is_err(),
                        fs::remove_file(&path).is_err(),
                    ]);
                }
            },
            &|| false,
        );
        result.expect("apply");
        assert_eq!(attempts, [[true; 3]]);
        assert_eq!(fs::read(&path).expect("patch"), TW_EX1_PATCH);
        assert_eq!(fs::read_to_string(fx.path(EX1_VER)).expect("ver"), EX1_VERSION);
    }
}
