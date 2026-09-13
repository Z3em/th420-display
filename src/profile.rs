use crate::config::Config;
use anyhow::{anyhow, bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

const ACTIVE_FILE: &str = "active-profile";

#[derive(Clone, Debug)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new() -> Self {
        let mut dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        dir.push("th420-display");
        dir.push("profiles");
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn active_path(&self) -> PathBuf {
        self.dir.parent().unwrap_or(&self.dir).join(ACTIVE_FILE)
    }

    pub fn sanitize_name(name: &str) -> String {
        let mut out = String::new();
        let mut last_sep = false;
        for c in name.trim().chars() {
            let ok = c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.');
            let c = if ok { c } else { '_' };
            let is_sep = c == ' ' || c == '_';
            if is_sep && last_sep {
                continue;
            }
            out.push(c);
            last_sep = is_sep;
        }
        let out = out
            .trim_matches(|c| matches!(c, ' ' | '.' | '_' | '-'))
            .to_string();
        if out.is_empty() {
            "Profile".to_string()
        } else {
            out
        }
    }

    fn profile_path(&self, name: &str) -> Result<PathBuf> {
        let safe = Self::sanitize_name(name);
        if safe != name.trim() {
            bail!("profile name contains unsupported characters; suggested name: {safe}");
        }
        Ok(self.dir.join(format!("{safe}.toml")))
    }

    pub fn list(&self) -> Result<Vec<String>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
        names.sort_by_key(|s| s.to_ascii_lowercase());
        Ok(names)
    }

    pub fn save(&self, name: &str, config: &Config) -> Result<()> {
        let path = self.profile_path(name)?;
        fs::create_dir_all(&self.dir)?;
        fs::write(path, toml::to_string_pretty(config)?)?;
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<Config> {
        let path = self.profile_path(name)?;
        let text = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&text)?;
        config.migrate_sensors();
        Ok(config)
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        let was_active = self.active_name().as_deref() == Some(old);
        let config = self.load(old)?;
        self.save(new, &config)?;
        self.delete(old)?;
        if was_active {
            self.set_active(new)?;
        }
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let path = self.profile_path(name)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        if self.active_name().as_deref() == Some(name) {
            let _ = fs::remove_file(self.active_path());
        }
        Ok(())
    }

    pub fn set_active(&self, name: &str) -> Result<()> {
        let _ = self.profile_path(name)?;
        let active_path = self.active_path();
        if let Some(parent) = active_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(active_path, name.trim())?;
        Ok(())
    }

    pub fn active_name(&self) -> Option<String> {
        let name = fs::read_to_string(self.active_path()).ok()?;
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    pub fn import(&self, source: &Path) -> Result<(String, Config)> {
        let text = fs::read_to_string(source)?;
        let mut config: Config = toml::from_str(&text)?;
        config.migrate_sensors();
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("profile has no valid file name"))?;
        let name = Self::sanitize_name(stem);
        self.save(&name, &config)?;
        Ok((name, config))
    }

    pub fn export(&self, name: &str, target: &Path) -> Result<()> {
        let source = self.profile_path(name)?;
        fs::copy(source, target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_filesystem_safe() {
        assert_eq!(ProfileStore::sanitize_name("Gaming / hot"), "Gaming hot");
        assert_eq!(ProfileStore::sanitize_name("  Hifumi...  "), "Hifumi");
        assert_eq!(ProfileStore::sanitize_name(""), "Profile");
    }
}
