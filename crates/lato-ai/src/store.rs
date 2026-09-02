use fs2::FileExt;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    Oauth {
        access: String,
        refresh: String,
        expires: i64,
        #[serde(default, alias = "accountId", skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

pub struct CredentialStore {
    pub path: PathBuf,
    data: serde_json::Map<String, serde_json::Value>,
}
impl CredentialStore {
    pub fn open(lato_home: &Path) -> io::Result<Self> {
        fs::create_dir_all(lato_home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(lato_home, fs::Permissions::from_mode(0o700))?;
        }
        let path = lato_home.join("auth.json");
        let data = if path.exists() {
            let text = fs::read_to_string(&path)?;
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            serde_json::Map::new()
        };
        Ok(Self { path, data })
    }
    pub fn get(&self, provider_id: &str) -> Option<Credential> {
        self.data
            .get(provider_id)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn contains(&self, provider_id: &str) -> bool {
        self.data.contains_key(provider_id)
    }
    pub fn modify<F: FnOnce(&mut serde_json::Map<String, serde_json::Value>)>(
        &mut self,
        f: F,
    ) -> io::Result<()> {
        let lock_path = self.path.with_extension("lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock_exclusive()?;
        if self.path.exists() {
            let text = fs::read_to_string(&self.path)?;
            self.data = serde_json::from_str(&text).unwrap_or_default();
        }
        f(&mut self.data);
        let bytes = serde_json::to_vec_pretty(&self.data).unwrap();
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &self.path)?;
        lock.unlock()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_credential_accepts_pi_account_id_alias() {
        let value = serde_json::json!({
            "type": "oauth",
            "access": "a",
            "refresh": "r",
            "expires": 42,
            "accountId": "acct-pi"
        });
        let credential: Credential = serde_json::from_value(value).unwrap();
        let Credential::Oauth { account_id, .. } = credential else {
            panic!()
        };
        assert_eq!(account_id.as_deref(), Some("acct-pi"));
    }
}
