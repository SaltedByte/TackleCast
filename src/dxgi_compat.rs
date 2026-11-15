use std::path::Path;
use tracing::warn;

/// Diagnose a possible local DXGI proxy without loading it or patching code.
/// Presence alone does not prove incompatibility and must not block startup.
pub fn ensure_dxgi_compatibility() {
    let result = std::env::current_exe()
        .map_err(|error| format!("could not locate executable: {error}"))
        .and_then(|exe| {
            exe.parent()
                .ok_or_else(|| "executable has no parent directory".to_string())
                .and_then(|directory| check_proxy_path(&directory.join("dxgi.dll")))
        });
    if let Err(message) = result {
        warn!("DXGI compatibility: {message}");
    }
}

fn check_proxy_path(path: &Path) -> Result<(), String> {
    match path.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => Err(format!(
            "possible local DXGI proxy at '{}'; automatic runtime patching is disabled. \
             If startup fails, disable ReShade or move the proxy DLL out of the application \
             directory and retry. Native DX12, FSR and RTX VSR remain available without the proxy",
            path.display()
        )),
        Err(error) => Err(format!(
            "could not inspect possible local DXGI proxy '{}': {error}; continuing without runtime patching",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::check_proxy_path;

    #[test]
    fn proxy_diagnostic_is_read_only() {
        let directory = std::env::temp_dir().join(format!(
            "tacklecast-dxgi-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("dxgi.dll");
        assert!(check_proxy_path(&path).is_ok());
        let sentinel = b"not a DLL: this file must never be loaded or modified";
        std::fs::write(&path, sentinel).unwrap();
        let diagnostic = check_proxy_path(&path).unwrap_err();
        assert!(diagnostic.contains("possible local DXGI proxy"));
        assert!(diagnostic.contains("automatic runtime patching is disabled"));
        assert_eq!(std::fs::read(&path).unwrap(), sentinel);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
