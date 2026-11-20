//! keyring 封装：service = "Ruthenium"，主机密码 account = `host:<host.id>`。
//!
//! 为什么加前缀：peer 身份密钥使用 account `peer-identity-v1`；若主机 id
//! 恰好等于该字符串（手改 hosts.toml / 导入备份都可能），保存主机密码会
//! 覆盖身份密钥，导致所有已配对设备失去信任。没有前缀的旧条目读取时保留
//! 一次回退，以兼容已有凭据。
use crate::error::{AppError, AppResult};

/// keyring service 名：所有条目共用。
pub const SERVICE: &str = "Ruthenium";

fn entry_for(account: &str) -> AppResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, account)
        .map_err(|e| AppError::ESecret(format!("创建钥匙串条目失败: {e}")))
}

fn entry(host_id: &str) -> AppResult<keyring::Entry> {
    entry_for(&format!("host:{host_id}"))
}

fn legacy_entry(host_id: &str) -> AppResult<keyring::Entry> {
    entry_for(host_id)
}

/// 把密码写入系统钥匙串；覆盖同 hostId 的旧值。
pub fn set_password(host_id: &str, password: &str) -> AppResult<()> {
    entry(host_id)?
        .set_password(password)
        .map_err(|e| AppError::ESecret(format!("保存密码到系统钥匙串失败: {e}")))?;
    // 写入新 account 后顺手清掉旧 account，避免同一主机存在两份密码。
    let _ = legacy_entry(host_id).and_then(|e| match e.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(AppError::ESecret(format!("清理旧钥匙串条目失败: {e}"))),
    });
    Ok(())
}

/// 查询该主机是否已保存密码（NoEntry 返回 false，其余错误向上返回）。
pub fn has_password(host_id: &str) -> AppResult<bool> {
    match entry(host_id)?.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => match legacy_entry(host_id)?.get_password() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(AppError::ESecret(format!("读取系统钥匙串失败: {e}"))),
        },
        Err(e) => Err(AppError::ESecret(format!("读取系统钥匙串失败: {e}"))),
    }
}

/// 读取密码；无条目返回 `Ok(None)`，**钥匙串不可用返回 Err**。
///
/// 「钥匙串故障」不能降级成 `Ok(None)`：那会让连接误报"未保存密码，请先输入密码"，
/// 用户反复输密码也不会成功（真实原因是读不到凭据管理器）。故障必须能区分出来，
/// 让 auth 层给出"系统凭据管理器不可用"的准确诊断。
pub fn get_password(host_id: &str) -> AppResult<Option<String>> {
    match entry(host_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => match legacy_entry(host_id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AppError::ESecret(format!("读取系统钥匙串失败: {e}"))),
        },
        Err(e) => Err(AppError::ESecret(format!("读取系统钥匙串失败: {e}"))),
    }
}

/// 删除该主机保存的密码；条目不存在视为成功。
pub fn delete_password(host_id: &str) -> AppResult<()> {
    for e in [entry(host_id), legacy_entry(host_id)] {
        match e?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(AppError::ESecret(format!("从系统钥匙串删除密码失败: {e}"))),
        }
    }
    Ok(())
}
