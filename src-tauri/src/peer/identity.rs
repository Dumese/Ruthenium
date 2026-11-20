//! 本机长期身份密钥（Ed25519）。
//!
//! 同一份密钥承担两个角色：
//! - **SSH 主机密钥**：对端连接本机时校验（钉扎在对方的 `peers.toml`）；
//! - **客户端身份密钥**：本机连接对端时用于 publickey 认证（对方按钉扎校验）。
//!
//! 存储：优先 Windows 凭据管理器（keyring，service `Ruthenium`、
//! account `peer-identity-v1`，OpenSSH PEM）；keyring 不可用时回退
//! `peer-identity.dpapi`（DPAPI 加密，当前用户绑定）。两处都失败则报错，
//! **不落明文**。
//!
//! 生成用 `ring::SystemRandom` 适配 `rand_core`（ssh-key 0.7 的 `PrivateKey::random`
//! 需要 `CryptoRng`；rand_core 0.10 没有现成 OsRng，故 20 行适配器）。

use crate::error::{AppError, AppResult};
use rand_core::{Infallible, TryCryptoRng, TryRng};
use ring::rand::SecureRandom;
use russh::keys::{ssh_key::LineEnding, Algorithm, HashAlg, PrivateKey, PublicKey};
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "Ruthenium";
const KEYRING_ACCOUNT: &str = "peer-identity-v1";
const DPAPI_FILE: &str = "peer-identity.dpapi";

pub struct Identity {
    key: PrivateKey,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
        }
    }
}

impl Identity {
    /// 读取已有身份；不存在则生成并保存。
    pub fn load_or_create() -> AppResult<Self> {
        // 1) keyring
        match keyring_entry() {
            Ok(entry) => match entry.get_password() {
                Ok(pem) => {
                    if let Ok(key) = PrivateKey::from_openssh(pem.as_bytes()) {
                        return Ok(Self { key });
                    }
                    applog!("[peer] keyring 中的身份密钥无法解析，按缺失处理并重新生成");
                }
                Err(_) => { /* 未保存过 */ }
            },
            Err(e) => applog!("[peer] keyring 不可用（{e}），尝试 DPAPI 文件回退"),
        }

        // 2) DPAPI 文件回退
        if let Ok(Some(key)) = load_dpapi() {
            return Ok(Self { key });
        }

        // 3) 生成新密钥
        let key = PrivateKey::random(&mut RingRng::new(), Algorithm::Ed25519)
            .map_err(|e| AppError::ESecret(format!("生成 Ed25519 身份密钥失败: {e}")))?;
        save_identity(&key)?;
        applog!(
            "[peer] 已生成新的身份密钥 {}",
            key.public_key().fingerprint(HashAlg::Sha256)
        );
        Ok(Self { key })
    }

    pub fn public_key(&self) -> &PublicKey {
        self.key.public_key()
    }

    /// OpenSSH 公钥行（`ssh-ed25519 AAAA... ruthenium-peer`），写入对端 `peers.toml`。
    pub fn public_line(&self) -> String {
        self.public_key()
            .to_openssh()
            .map(|s| format!("{s} ruthenium-peer"))
            .unwrap_or_default()
    }

    /// `SHA256:base64` 指纹（UI 展示 / 审计）。
    pub fn fingerprint(&self) -> String {
        self.public_key().fingerprint(HashAlg::Sha256).to_string()
    }

    pub fn private_key(&self) -> &PrivateKey {
        &self.key
    }
}

fn keyring_entry() -> AppResult<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &keyring_account()).map_err(AppError::from)
}

/// 凭据管理器账户名。
///
/// 默认 `peer-identity-v1`；设置 `RUTHENIUM_DATA_DIR` 时按数据目录加固定后缀——
/// 同机双实例（测试/开发）必须拥有**不同的设备身份**，否则配对时会把自己当成对方。
/// 生产路径（无该环境变量）永远使用稳定账户名，密钥跨版本可复用。
fn keyring_account() -> String {
    account_for(std::env::var_os("RUTHENIUM_DATA_DIR").as_deref())
}

fn account_for(data_dir: Option<&std::ffi::OsStr>) -> String {
    if let Some(dir) = data_dir {
        let dir = dir.to_string_lossy();
        if !dir.is_empty() {
            let digest = ring::digest::digest(&ring::digest::SHA256, dir.as_bytes());
            let hex: String = digest.as_ref()[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            return format!("{KEYRING_ACCOUNT}-{hex}");
        }
    }
    KEYRING_ACCOUNT.to_string()
}

fn save_identity(key: &PrivateKey) -> AppResult<()> {
    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| AppError::ESecret(format!("导出身份密钥失败: {e}")))?;
    // 先试 keyring；失败再写 DPAPI 文件，二者都失败才报错
    if let Ok(entry) = keyring_entry() {
        if entry.set_password(&pem).is_ok() {
            return Ok(());
        }
    }
    save_dpapi(pem.as_bytes())
}

#[cfg(windows)]
fn dpapi_path() -> AppResult<std::path::PathBuf> {
    Ok(crate::config::config_dir()?.join(DPAPI_FILE))
}

#[cfg(windows)]
fn save_dpapi(plain: &[u8]) -> AppResult<()> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| AppError::ESecret(format!("DPAPI 加密失败: {e}")))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        std::fs::write(dpapi_path()?, bytes)
            .map_err(|e| AppError::ESecret(format!("写入身份密钥文件失败: {e}")))
    }
}

#[cfg(windows)]
fn load_dpapi() -> AppResult<Option<PrivateKey>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let path = match dpapi_path() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    let enc = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: enc.len() as u32,
            pbData: enc.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        if CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .is_err()
        {
            applog!("[peer] DPAPI 解密身份密钥失败（可能换了用户/机器）");
            return Ok(None);
        }
        let plain =
            Zeroizing::new(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec());
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        Ok(PrivateKey::from_openssh(plain.as_slice()).ok())
    }
}

#[cfg(not(windows))]
fn save_dpapi(_plain: &[u8]) -> AppResult<()> {
    Err(AppError::ESecret("当前平台没有 DPAPI 回退".to_string()))
}

#[cfg(not(windows))]
fn load_dpapi() -> AppResult<Option<PrivateKey>> {
    Ok(None)
}

/// rand_core 0.10 → ring SystemRandom 适配（ssh-key 的 `PrivateKey::random` 需要）。
struct RingRng(ring::rand::SystemRandom);

impl RingRng {
    fn new() -> Self {
        Self(ring::rand::SystemRandom::new())
    }
}

impl TryRng for RingRng {
    type Error = Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        self.0.fill(&mut b).expect("系统 RNG 不可用");
        Ok(u32::from_ne_bytes(b))
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        self.0.fill(&mut b).expect("系统 RNG 不可用");
        Ok(u64::from_ne_bytes(b))
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        self.0.fill(dst).expect("系统 RNG 不可用");
        Ok(())
    }
}

impl TryCryptoRng for RingRng {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn keyring_account_is_stable_and_namespaced_for_test_dirs() {
        // 生产路径：稳定账户名（跨版本复用密钥）
        assert_eq!(account_for(None), "peer-identity-v1");
        assert_eq!(account_for(Some(OsStr::new(""))), "peer-identity-v1");
        // 测试/开发：不同数据目录 → 不同设备身份；同一目录 → 稳定
        let a = account_for(Some(OsStr::new(r"C:\tmp\peer-a")));
        let a2 = account_for(Some(OsStr::new(r"C:\tmp\peer-a")));
        let b = account_for(Some(OsStr::new(r"C:\tmp\peer-b")));
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert!(a.starts_with("peer-identity-v1-"));
    }
}
