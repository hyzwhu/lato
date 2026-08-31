use std::ffi::OsString;

pub fn default_shell() -> OsString {
    #[cfg(windows)]
    {
        if which("pwsh") {
            return OsString::from("pwsh");
        }
        OsString::from("powershell.exe")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("bash"))
    }
}

#[cfg(windows)]
fn which(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a3_5_default_shell() {
        let sh = default_shell();
        #[cfg(windows)]
        {
            let s = sh.to_string_lossy().to_lowercase();
            assert!(s.contains("pwsh") || s.contains("powershell"), "{s}");
            assert!(!s.contains("bash"));
        }
        #[cfg(not(windows))]
        {
            let s = sh.to_string_lossy();
            assert!(!s.is_empty());
        }
    }
}
