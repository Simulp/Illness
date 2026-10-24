use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::Rgb;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub accent: Rgb,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Command Prompt".to_string(),
            program: "cmd.exe".to_string(),
            args: Vec::new(),
            accent: Rgb::new(0xe3, 0xb3, 0x41),
        }
    }
}

/// Detect installed shells without executing anything.
///
/// Order is preference order: PowerShell 7, Windows PowerShell, Command Prompt, WSL.
pub fn detect_profiles() -> Vec<Profile> {
    let mut profiles = Vec::new();

    if let Some(program_files) = env::var_os("ProgramFiles") {
        let pwsh = PathBuf::from(program_files)
            .join("PowerShell")
            .join("7")
            .join("pwsh.exe");
        if pwsh.is_file() {
            push_unique(
                &mut profiles,
                Profile {
                    name: "PowerShell".to_string(),
                    program: path_string(&pwsh),
                    args: Vec::new(),
                    accent: Rgb::new(0x7e, 0xe7, 0x87),
                },
            );
        }
    }

    if let Some(system_root) = env::var_os("SystemRoot") {
        let powershell = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if powershell.is_file() {
            push_unique(
                &mut profiles,
                Profile {
                    name: "Windows PowerShell".to_string(),
                    program: path_string(&powershell),
                    args: Vec::new(),
                    accent: Rgb::new(0x6c, 0xb6, 0xff),
                },
            );
        }
    }

    push_unique(
        &mut profiles,
        Profile {
            name: "Command Prompt".to_string(),
            program: command_prompt_program(),
            args: Vec::new(),
            accent: Rgb::new(0xe3, 0xb3, 0x41),
        },
    );

    if let Some(wsl) = find_on_path("wsl.exe") {
        let has_distro = std::process::Command::new(&wsl)
            .args(["--list", "--quiet"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
            });
        if has_distro {
            push_unique(
                &mut profiles,
                Profile {
                    name: "WSL".to_string(),
                    program: path_string(&wsl),
                    args: Vec::new(),
                    accent: Rgb::new(0xe9, 0x54, 0x20),
                },
            );
        }
    }

    profiles
}

fn push_unique(profiles: &mut Vec<Profile>, profile: Profile) {
    if !profiles
        .iter()
        .any(|existing| existing.name.eq_ignore_ascii_case(&profile.name))
    {
        profiles.push(profile);
    }
}

fn command_prompt_program() -> String {
    if let Some(comspec) = env::var_os("COMSPEC") {
        let path = PathBuf::from(comspec);
        let is_cmd = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"));
        if is_cmd {
            return path_string(&path);
        }
    }

    if let Some(system_root) = env::var_os("SystemRoot") {
        let cmd = PathBuf::from(system_root).join("System32").join("cmd.exe");
        if cmd.is_file() {
            return path_string(&cmd);
        }
    }

    "cmd.exe".to_string()
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_command_prompt_always() {
        let profiles = detect_profiles();
        let command_prompt = profiles
            .iter()
            .find(|profile| profile.name == "Command Prompt")
            .expect("Command Prompt must always be detected");

        assert!(
            command_prompt
                .program
                .to_ascii_lowercase()
                .ends_with("cmd.exe"),
            "unexpected cmd program: {}",
            command_prompt.program
        );
    }

    #[test]
    fn profile_names_are_unique() {
        let profiles = detect_profiles();
        let mut names: Vec<&str> = profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), profiles.len());
    }

    #[test]
    fn profile_names_are_known() {
        let known = ["PowerShell", "Windows PowerShell", "Command Prompt", "WSL"];
        for profile in detect_profiles() {
            assert!(
                known.contains(&profile.name.as_str()),
                "unexpected profile: {}",
                profile.name
            );
            assert!(!profile.program.is_empty());
        }
    }
}
