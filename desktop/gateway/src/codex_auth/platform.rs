use std::path::PathBuf;

pub(super) const fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

pub(super) fn browser_open_bin() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/usr/bin/xdg-open");
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from("/usr/bin/open")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_build_platform_matches_support_contract() {
        assert_eq!(
            super::supported(),
            cfg!(any(target_os = "macos", target_os = "linux"))
        );
    }
}
