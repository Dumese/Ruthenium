//! 秘密命令：keyring 密码存取。
use crate::error::{AppError, AppResult};
use crate::secret;
use crate::state::AppState;

/// keyring 是同步阻塞 API：统一放到阻塞线程池执行，避免卡住 tokio worker。
async fn blocking_keyring<T>(f: impl FnOnce() -> AppResult<T> + Send + 'static) -> AppResult<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::ESecret(format!("钥匙串任务执行失败: {e}")))?
}

/// 把密码写入系统凭据管理器（按 hostId 存，不落盘、不回传）。
#[tauri::command]
pub async fn set_password(
    _state: tauri::State<'_, AppState>,
    host_id: String,
    password: String,
) -> AppResult<()> {
    blocking_keyring(move || secret::set_password(&host_id, &password)).await
}

/// 查询该主机是否已保存密码。
#[tauri::command]
pub async fn has_password(_state: tauri::State<'_, AppState>, host_id: String) -> AppResult<bool> {
    blocking_keyring(move || secret::has_password(&host_id)).await
}

/// 删除该主机保存的密码；条目不存在视为成功。
#[tauri::command]
pub async fn delete_password(_state: tauri::State<'_, AppState>, host_id: String) -> AppResult<()> {
    blocking_keyring(move || secret::delete_password(&host_id)).await
}
