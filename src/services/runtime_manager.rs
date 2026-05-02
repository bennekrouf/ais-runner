use std::path::PathBuf;
#[cfg(feature = "bundle-azurite")]
use std::fs;
use std::env;

/// Resolves the path to a tool (like azurite or func).
/// It first checks if a bundled version exists in the 'bin' folder next to the app executable.
/// If not found, it returns the tool name as-is (falling back to the system PATH).
pub fn resolve_tool(name: &str) -> String {
    // 1. Special case for Azurite: Try extracting the embedded binary (Single-EXE mode)
    if name == "azurite" {
        if let Ok(path) = extract_embedded_azurite() {
            return path;
        }
    }

    // 2. Check next to the executable (Sidecar mode)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let bundled = parent.join("bin").join(if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() });
            if bundled.exists() {
                return bundled.to_str().unwrap_or(name).to_string();
            }
        }
    }

    // 3. Check in the project root bin/ (Development mode)
    // We use a macro to get the manifest dir at compile time
    let dev_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bin")
        .join(if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() });
    
    if dev_bin.exists() {
        return dev_bin.to_str().unwrap_or(name).to_string();
    }

    // 4. Fallback to system PATH
    name.to_string()
}

/// Extracts the embedded Azurite binary to a temp folder if it exists.
/// This enables "Single-EXE" distribution.
fn extract_embedded_azurite() -> Result<String, String> {
    #[cfg(feature = "bundle-azurite")]
    {
        // We use include_bytes! to bake the binary into our app.
        // bin/azurite must exist during compilation for this to work.
        const BYTES: &[u8] = include_bytes!("../../bin/azurite");
        
        let mut temp_path = env::temp_dir();
        // Use a versioned name so updates trigger a re-extraction
        temp_path.push(format!("ais_runner_azurite_{}", BYTES.len()));
        if cfg!(windows) { temp_path.set_extension("exe"); }

        if !temp_path.exists() || fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0) != BYTES.len() as u64 {
            fs::write(&temp_path, BYTES).map_err(|e| e.to_string())?;
            
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755)).ok();
            }
        }
        return Ok(temp_path.to_str().unwrap().to_string());
    }

    #[cfg(not(feature = "bundle-azurite"))]
    Err("Embedded binary not enabled in this build".to_string())
}

