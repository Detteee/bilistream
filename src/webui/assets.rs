use std::path::Path;

// Missing assets must match the running binary, including feature branches.
// Downloading just index.html from main could leave an unbootable module graph.
macro_rules! asset {
    ($path:literal) => {
        ($path, include_bytes!(concat!("../../", $path)).as_slice())
    };
}

pub(crate) const BUNDLED_ASSETS: &[(&str, &[u8])] = &[
    asset!("webui/dist/index.html"),
    asset!("webui/dist/styles.css"),
    asset!("webui/dist/js/main.js"),
    asset!("webui/dist/js/api.js"),
    asset!("webui/dist/js/config-draft.js"),
    asset!("webui/dist/js/crop.js"),
    asset!("webui/dist/js/dialog.js"),
    asset!("webui/dist/js/dom.js"),
    asset!("webui/dist/js/events.js"),
    asset!("webui/dist/js/format.js"),
    asset!("webui/dist/js/logs.js"),
    asset!("webui/dist/js/manage.js"),
    asset!("webui/dist/js/overview.js"),
    asset!("webui/dist/js/settings.js"),
    asset!("webui/dist/js/setup.js"),
    asset!("webui/dist/js/state.js"),
    asset!("webui/dist/js/status-cards.js"),
    asset!("webui/dist/js/toggle-save.js"),
];

pub(crate) fn missing_asset_count(directory: &Path) -> usize {
    BUNDLED_ASSETS
        .iter()
        .filter(|(path, _)| !directory.join(path).is_file())
        .count()
}

pub(crate) fn install_missing_assets(directory: &Path) -> std::io::Result<usize> {
    let mut installed = 0;
    for (relative, bytes) in BUNDLED_ASSETS {
        let path = directory.join(relative);
        if path.is_file() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::config::write_file_atomic(&path, bytes)?;
        installed += 1;
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_partial_installation_without_overwriting_existing_assets() {
        let directory =
            std::env::temp_dir().join(format!("bilistream-assets-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("webui/dist")).unwrap();
        let index = directory.join("webui/dist/index.html");
        std::fs::write(&index, b"existing installation").unwrap();
        install_missing_assets(&directory).unwrap();
        assert_eq!(std::fs::read(&index).unwrap(), b"existing installation");
        assert_eq!(missing_asset_count(&directory), 0);
        assert_eq!(install_missing_assets(&directory).unwrap(), 0);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_admin_asset_is_bundled() {
        fn check(directory: &Path) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    check(&path);
                } else {
                    let relative = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                    assert!(
                        BUNDLED_ASSETS
                            .iter()
                            .any(|(name, _)| Path::new(name) == relative),
                        "unbundled asset: {relative:?}"
                    );
                }
            }
        }
        check(&Path::new(env!("CARGO_MANIFEST_DIR")).join("webui/dist"));
    }
}
