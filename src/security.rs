use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rustypipe::cache::CacheStorage;
use zeroize::{Zeroize, Zeroizing};

pub const ENCRYPTED_SESSION_FILE: &str = "session.dpapi";
pub const TEMP_AUTH_PROFILE_PREFIX: &str = "auth_tmp_";
const LEGACY_CACHE_FILE: &str = "rustypipe_cache.json";

/// Entropía específica de YT-Niusic para Windows DPAPI (defensa en profundidad).
#[cfg(windows)]
const DPAPI_ENTROPY: &[u8] = b"YT-Niusic::DPAPI::SessionCache::v1";

/// Lista blanca de cookies de mínimo privilegio requeridas por `music.youtube.com` / RustyPipe.
/// Cualquier otra cookie (tracking, publicidad, otros dominios Google) se descarta antes de guardar.
const ALLOWED_YTM_COOKIES: &[&str] = &[
    "SAPISID",
    "APISID",
    "SID",
    "HSID",
    "SSID",
    "LOGIN_INFO",
    "SIDCC",
    "SOCS",
    "OSID",
    "__Secure-OSID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
    "__Secure-1PSIDTS",
    "__Secure-3PSIDTS",
    "__Secure-1PSIDCC",
    "__Secure-3PSIDCC",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieFilterStats {
    pub kept: usize,
    pub dropped: usize,
    pub is_netscape: bool,
}

/// Filtra una entrada de cookies (formato Netscape `cookies.txt` o cabecera `Cookie: ...`)
/// conservando únicamente las cookies mínimas necesarias para YouTube Music.
pub fn filter_minimal_ytm_cookies(raw: &str) -> (Zeroizing<String>, CookieFilterStats) {
    let trimmed = raw.trim();
    let is_netscape = trimmed.starts_with("# Netscape HTTP Cookie File")
        || trimmed.starts_with("# HTTP Cookie File")
        || trimmed.lines().any(|line| {
            let l = line.trim();
            !l.is_empty() && !l.starts_with('#') && l.split('\t').count() >= 7
        });

    let mut kept = 0usize;
    let mut dropped = 0usize;

    if is_netscape {
        let mut out = String::from("# Netscape HTTP Cookie File\n");
        for line in trimmed.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = l.split('\t').collect();
            if cols.len() < 7 {
                continue;
            }
            let domain = cols[0].trim().to_ascii_lowercase();
            let name = cols[5].trim();
            let is_yt_domain = domain == ".youtube.com"
                || domain == "youtube.com"
                || domain == "music.youtube.com"
                || domain == ".music.youtube.com"
                || domain == "www.youtube.com";

            if is_yt_domain && is_allowed_cookie_name(name) {
                out.push_str(l);
                out.push('\n');
                kept += 1;
            } else {
                dropped += 1;
            }
        }
        (
            Zeroizing::new(out),
            CookieFilterStats {
                kept,
                dropped,
                is_netscape: true,
            },
        )
    } else {
        let cleaned = trimmed
            .strip_prefix("Cookie:")
            .or_else(|| trimmed.strip_prefix("cookie:"))
            .unwrap_or(trimmed)
            .trim();

        let mut pairs = Vec::new();
        for part in cleaned.split(';') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if let Some((name, _val)) = p.split_once('=') {
                if is_allowed_cookie_name(name.trim()) {
                    pairs.push(p);
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }

        (
            Zeroizing::new(pairs.join("; ")),
            CookieFilterStats {
                kept,
                dropped,
                is_netscape: false,
            },
        )
    }
}

fn is_allowed_cookie_name(name: &str) -> bool {
    ALLOWED_YTM_COOKIES.contains(&name)
}

/// Sobrescribe un archivo sensible con ceros y luego lo elimina del disco.
pub fn secure_delete_file(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if let Ok(meta) = fs::metadata(path) {
        let len = meta.len() as usize;
        if len > 0 {
            if let Ok(mut f) = OpenOptions::new().write(true).open(path) {
                let zeros = vec![0u8; len.min(1024 * 1024)];
                let mut remaining = len;
                while remaining > 0 {
                    let chunk = remaining.min(zeros.len());
                    if f.write_all(&zeros[..chunk]).is_err() {
                        break;
                    }
                    remaining -= chunk;
                }
                let _ = f.sync_all();
            }
        }
    }
    fs::remove_file(path)
}

/// Limpia de forma segura un directorio temporal de perfil de navegador usado durante el login.
/// Sobrescribe primero las bases de datos de cookies conocidas y reintenta brevemente si Windows
/// mantiene algún manejador abierto unos milisegundos tras cerrar el navegador.
pub fn secure_cleanup_auth_profile(profile_dir: &Path) {
    if !profile_dir.exists() {
        return;
    }

    let sensitive_rel_paths = [
        "Default/Network/Cookies",
        "Default/Network/Cookies-journal",
        "Default/Cookies",
        "Default/Cookies-journal",
        "Local State",
    ];
    for rel in sensitive_rel_paths {
        let target = profile_dir.join(rel);
        if target.exists() {
            let _ = secure_delete_file(&target);
        }
    }

    for attempt in 0..6 {
        if fs::remove_dir_all(profile_dir).is_ok() || !profile_dir.exists() {
            break;
        }
        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

/// Elimina cualquier carpeta temporal `auth_tmp_*` huérfana de ejecuciones anteriores
/// (por ejemplo, si la aplicación se cerró abruptamente en medio de un inicio de sesión).
pub fn cleanup_stale_auth_profiles(data_dir: &Path) {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(TEMP_AUTH_PROFILE_PREFIX)
        {
            secure_cleanup_auth_profile(&path);
        }
    }
}

/// Backend de almacenamiento cifrado para RustyPipe usando Windows DPAPI.
///
/// - Vincula el cifrado exclusivamente al usuario actual de Windows en esta PC.
/// - Usa escritura atómica (`.tmp` + rename) para evitar corrupción.
/// - Migra y destruye automáticamente cualquier `rustypipe_cache.json` plano anterior.
pub struct EncryptedCacheStorage {
    encrypted_path: PathBuf,
}

impl EncryptedCacheStorage {
    pub fn new(data_dir: &Path) -> Self {
        let encrypted_path = data_dir.join(ENCRYPTED_SESSION_FILE);
        let legacy_path = data_dir.join(LEGACY_CACHE_FILE);

        let storage = Self { encrypted_path };

        // Migración automática y borrado seguro de cualquier caché plano heredado
        if legacy_path.exists() {
            if !storage.encrypted_path.exists() {
                if let Ok(mut plaintext) = fs::read_to_string(&legacy_path) {
                    storage.write(&plaintext);
                    plaintext.zeroize();
                }
            }
            let _ = secure_delete_file(&legacy_path);
        }

        storage
    }

    pub fn encrypted_path(&self) -> &Path {
        &self.encrypted_path
    }
}

impl CacheStorage for EncryptedCacheStorage {
    fn write(&self, data: &str) {
        let ciphertext = match dpapi_protect(data.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("No se pudo cifrar la caché local con DPAPI: {e}");
                return;
            }
        };

        let tmp_path = self.encrypted_path.with_extension("dpapi.tmp");
        let res = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(&ciphertext)?;
            f.sync_all()?;
            drop(f);
            // En Windows, rename falla si el destino existe; usamos reemplazo seguro
            if self.encrypted_path.exists() {
                let _ = fs::remove_file(&self.encrypted_path);
            }
            fs::rename(&tmp_path, &self.encrypted_path)?;
            Ok(())
        })();

        if let Err(e) = res {
            tracing::error!("No se pudo guardar la sesión cifrada en disco: {e}");
            let _ = fs::remove_file(&tmp_path);
        }
    }

    fn read(&self) -> Option<String> {
        if !self.encrypted_path.exists() {
            return None;
        }
        let ciphertext = match fs::read(&self.encrypted_path) {
            Ok(b) if !b.is_empty() => b,
            _ => return None,
        };

        match dpapi_unprotect(&ciphertext) {
            Ok(mut plain_bytes) => {
                let text = String::from_utf8(plain_bytes.clone()).ok();
                plain_bytes.zeroize();
                text
            }
            Err(e) => {
                tracing::warn!("No se pudo descifrar la sesión DPAPI local: {e}");
                None
            }
        }
    }
}

#[cfg(windows)]
fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let data_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(plaintext.len()).map_err(|_| "Datos demasiado grandes".to_string())?,
        pbData: plaintext.as_ptr() as *mut u8,
    };

    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr() as *mut u8,
    };

    let mut data_out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };

    // SAFETY: Punteros válidos por la duración de la llamada; `data_out.pbData` se libera con LocalFree.
    let ok = unsafe {
        CryptProtectData(
            &data_in,
            ptr::null(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut data_out,
        )
    };

    if ok == 0 || data_out.pbData.is_null() {
        return Err("CryptProtectData devolvió error".to_string());
    }

    let result = unsafe {
        let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
        let vec = slice.to_vec();
        LocalFree(data_out.pbData as *mut core::ffi::c_void);
        vec
    };

    Ok(result)
}

#[cfg(windows)]
fn dpapi_unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let data_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(ciphertext.len())
            .map_err(|_| "Blob cifrado demasiado grande".to_string())?,
        pbData: ciphertext.as_ptr() as *mut u8,
    };

    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr() as *mut u8,
    };

    let mut data_out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };

    // SAFETY: Punteros válidos; limpiamos el buffer descifrado con ceros antes de llamar a LocalFree.
    let ok = unsafe {
        CryptUnprotectData(
            &data_in,
            ptr::null_mut(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut data_out,
        )
    };

    if ok == 0 || data_out.pbData.is_null() {
        return Err("CryptUnprotectData devolvió error".to_string());
    }

    let result = unsafe {
        let slice = std::slice::from_raw_parts_mut(data_out.pbData, data_out.cbData as usize);
        let vec = slice.to_vec();
        slice.zeroize();
        LocalFree(data_out.pbData as *mut core::ffi::c_void);
        vec
    };

    Ok(result)
}

#[cfg(not(windows))]
fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Ok(plaintext.to_vec())
}

#[cfg(not(windows))]
fn dpapi_unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    Ok(ciphertext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip_and_no_plaintext_leak() {
        let tmp = std::env::temp_dir().join("yt_niusic_dpapi_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let secret_json = r#"{"auth_cookie":{"cookie":"SAPISID=super_secret_token_12345; SID=abc"}}"#;
        let storage = EncryptedCacheStorage::new(&tmp);
        storage.write(secret_json);

        // El archivo en disco NO debe contener texto plano
        let raw_disk = fs::read(storage.encrypted_path()).unwrap();
        assert!(!raw_disk.is_empty());
        assert!(
            !raw_disk
                .windows(b"super_secret_token".len())
                .any(|w| w == b"super_secret_token"),
            "El archivo DPAPI no debe contener texto plano"
        );

        // Al leer de nuevo debe descifrar exactamente lo guardado
        let restored = storage.read().expect("should decrypt DPAPI cache");
        assert_eq!(restored, secret_json);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn legacy_plaintext_cache_is_migrated_and_deleted() {
        let tmp = std::env::temp_dir().join("yt_niusic_dpapi_migrate_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let legacy = tmp.join(LEGACY_CACHE_FILE);
        fs::write(&legacy, r#"{"migrated":"yes_secret"}"#).unwrap();
        assert!(legacy.exists());

        let storage = EncryptedCacheStorage::new(&tmp);
        assert!(!legacy.exists(), "El caché JSON plano debe eliminarse");
        assert!(storage.encrypted_path().exists());
        assert_eq!(storage.read().as_deref(), Some(r#"{"migrated":"yes_secret"}"#));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cookie_filter_strips_non_essential_cookies() {
        let header = "Cookie: VISITOR_INFO1_LIVE=xyz; SAPISID=keep1; PREF=f1=500; __Secure-3PSID=keep2; NID=track; SID=keep3";
        let (filtered, stats) = filter_minimal_ytm_cookies(header);
        assert!(!stats.is_netscape);
        assert_eq!(stats.kept, 3);
        assert_eq!(stats.dropped, 3);
        assert!(filtered.contains("SAPISID=keep1"));
        assert!(filtered.contains("__Secure-3PSID=keep2"));
        assert!(filtered.contains("SID=keep3"));
        assert!(!filtered.contains("NID="));
        assert!(!filtered.contains("PREF="));

        let netscape = "\
# Netscape HTTP Cookie File
.google.com\tTRUE\t/\tTRUE\t0\tNID\ttracking_val
.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tkeep_yt
.youtube.com\tTRUE\t/\tTRUE\t0\tVISITOR_INFO1_LIVE\ttracker_yt
music.youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tkeep_music
";
        let (filtered_n, stats_n) = filter_minimal_ytm_cookies(netscape);
        assert!(stats_n.is_netscape);
        assert_eq!(stats_n.kept, 2);
        assert_eq!(stats_n.dropped, 2);
        assert!(filtered_n.contains("SAPISID\tkeep_yt"));
        assert!(filtered_n.contains("__Secure-3PAPISID\tkeep_music"));
        assert!(!filtered_n.contains("tracking_val"));
        assert!(!filtered_n.contains("tracker_yt"));
    }

    #[test]
    fn stale_auth_profiles_are_cleaned_up() {
        let tmp = std::env::temp_dir().join("yt_niusic_stale_profile_test");
        let _ = fs::remove_dir_all(&tmp);
        let stale = tmp.join(format!("{TEMP_AUTH_PROFILE_PREFIX}12345"));
        let keep = tmp.join("keep_dir");
        fs::create_dir_all(stale.join("Default/Network")).unwrap();
        fs::create_dir_all(&keep).unwrap();
        fs::write(stale.join("Default/Network/Cookies"), b"sensitive_cookie_bytes").unwrap();
        fs::write(keep.join("data.txt"), b"keep").unwrap();

        cleanup_stale_auth_profiles(&tmp);

        assert!(!stale.exists(), "El perfil temporal huérfano debe eliminarse");
        assert!(keep.exists(), "Otros directorios no deben tocarse");

        let _ = fs::remove_dir_all(&tmp);
    }
}
