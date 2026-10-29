//! 「角色」分頁的 commands。共用狀態在 `commands::AppState`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::State;

use super::usage::Usage;
use super::{self as characters, BackupKind, Category, CategoryPlan, CharacterInfo, TransferResult};
use crate::commands::{AppState, update_config};
use crate::error::{AppError, AppResult};
use crate::region::Region;
use crate::store;

/// 角色清單裡的一個角色：資料夾資訊＋自訂名稱＋使用標記。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterView {
    #[serde(flatten)]
    pub info: CharacterInfo,
    pub name: Option<String>,
    pub usage: Option<Usage>,
}

/// 一個區服的角色；`exists` 為 false＝區服資料夾還不存在（遊戲沒在這台電腦啟動過）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionCharacters {
    pub dir: String,
    pub exists: bool,
    pub characters: Vec<CharacterView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterList {
    pub global: RegionCharacters,
    pub tw: RegionCharacters,
}

/// 一個角色：區服＋16 位 hex id（protocols §7.3）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterKey {
    pub region: Region,
    pub id: String,
}

impl CharacterKey {
    /// 驗證 id 後回 (區服資料夾, 角色資料夾)；角色資料夾不存在即失敗。
    fn dirs(&self) -> AppResult<(PathBuf, PathBuf)> {
        characters::check_id(&self.id)?;
        let game_dir = crate::game::config_dir(self.region)?;
        let dir = characters::character_dir(&game_dir, &self.id);
        if !dir.is_dir() {
            return Err(AppError::NotFound {
                what: format!("{}角色資料夾 {}", self.region, dir.display()),
            });
        }
        Ok((game_dir, dir))
    }

    fn same_as(&self, other: &CharacterKey) -> bool {
        self.region == other.region && self.id.eq_ignore_ascii_case(&other.id)
    }
}

/// 「角色」分頁：兩服的角色、自訂名稱、使用標記與備份。只讀，不經 launch_lock。
#[tauri::command]
pub async fn list_characters(state: State<'_, AppState>) -> AppResult<CharacterList> {
    let config = {
        let _guard = state.config_lock.lock().await;
        store::load_config(&state.paths)?
    };
    let usage = characters::usage::load(&state.paths)?;
    let of_region = |region: Region| -> AppResult<RegionCharacters> {
        let dir = crate::game::config_dir(region)?;
        let names = config.characters.get(region);
        let marks = usage.get(region);
        let characters = characters::list(&dir, &state.paths.character_backups_dir(region))?
            .into_iter()
            .map(|info| CharacterView {
                name: names.get(&info.id).cloned(),
                usage: marks.get(&info.id).cloned(),
                info,
            })
            .collect();
        Ok(RegionCharacters {
            exists: dir.is_dir(),
            dir: dir.display().to_string(),
            characters,
        })
    };
    Ok(CharacterList {
        global: of_region(Region::Global)?,
        tw: of_region(Region::Tw)?,
    })
}

/// 設定角色的自訂名稱；空白＝刪除。
#[tauri::command]
pub async fn set_character_label(
    state: State<'_, AppState>,
    region: Region,
    id: String,
    name: String,
) -> AppResult<()> {
    characters::check_id(&id)?;
    update_config(&state, |c| {
        let names = c.characters.get_mut(region);
        match name.trim() {
            "" => names.remove(&id),
            name => names.insert(id, name.to_string()),
        };
        Ok(())
    })
    .await
}

/// 手動備份（只讀遊戲檔，遊戲執行中也可以）；回備份 id。
#[tauri::command]
pub async fn backup_character(state: State<'_, AppState>, region: Region, id: String) -> AppResult<String> {
    let _guard = state.launch_lock.lock().await;
    let (game_dir, _) = CharacterKey { region, id: id.clone() }.dirs()?;
    characters::backup(&game_dir, &state.paths.character_backups_dir(region), &id, BackupKind::Manual)
}

/// 把角色還原成 `backup_id`（寫入互斥檢查 → 自動備份 → 寫回）；回寫入前的自動備份 id。
#[tauri::command]
pub async fn restore_character(
    state: State<'_, AppState>,
    region: Region,
    id: String,
    backup_id: String,
) -> AppResult<String> {
    let _guard = state.launch_lock.lock().await;
    let (game_dir, _) = CharacterKey { region, id: id.clone() }.dirs()?;
    characters::ensure_writable(&store::load_config(&state.paths)?, region)?;
    characters::restore(&game_dir, &state.paths.character_backups_dir(region), &id, &backup_id)
}

/// 轉移計畫（只讀）：每個分類「複製／合併／不轉＋原因」。
#[tauri::command]
pub async fn plan_character_transfer(from: CharacterKey, to: CharacterKey) -> AppResult<Vec<CategoryPlan>> {
    if from.same_as(&to) {
        return Err(AppError::TransferSameCharacter);
    }
    let (_, source) = from.dirs()?;
    let (_, target) = to.dirs()?;
    characters::plan(&source, &target, from.region == to.region)
}

/// 轉移：寫入互斥檢查（目標區服）→ 重算計畫，只做 `categories` 中可轉的 → 自動備份目標 → 寫入。
#[tauri::command]
pub async fn transfer_character(
    state: State<'_, AppState>,
    from: CharacterKey,
    to: CharacterKey,
    categories: Vec<Category>,
) -> AppResult<TransferResult> {
    if from.same_as(&to) {
        return Err(AppError::TransferSameCharacter);
    }
    let _guard = state.launch_lock.lock().await;
    let (_, source) = from.dirs()?;
    let (game_dir, _) = to.dirs()?;
    characters::ensure_writable(&store::load_config(&state.paths)?, to.region)?;
    characters::transfer(
        &source,
        &game_dir,
        &state.paths.character_backups_dir(to.region),
        &to.id,
        from.region == to.region,
        &categories,
    )
}
