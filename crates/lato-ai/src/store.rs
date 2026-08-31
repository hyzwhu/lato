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
    },
}

pub struct CredentialStore {
    pub path: PathBuf,
    data: serde_json::Map<String, serde_json::Value>,
}
impl CredentialStore {
    pub fn open(lato_home: &Path) -> io::Result<Self> {
        fs::create_dir_all(lato_home)?;
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
    pub fn modify<F: FnOnce(&mut serde_json::Map<String, serde_json::Value>)>(
        &mut self,
        f: F,
    ) -> io::Result<()> {
        f(&mut self.data);
        let bytes = serde_json::to_vec_pretty(&self.data).unwrap();
        fs::write(&self.path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}
