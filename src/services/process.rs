use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone, PartialEq, Debug)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
}

/// A managed background process (Azurite or func start).
pub struct ManagedProcess {
    pub child: Arc<Mutex<Option<Child>>>,
}

impl ManagedProcess {
    pub fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
        }
    }



    pub fn start(&self, program: &str, args: &[&str], workdir: Option<&str>) -> Result<(), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("Process already started".into());
        }
        let mut cmd = Command::new(program);
        cmd.args(args)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn '{}': {}", program, e))?;
        *guard = Some(child);
        Ok(())
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.take() {
            child.kill().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
