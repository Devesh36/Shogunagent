//! Read-only discovery of local AI tools. Presence means installed, never connected to SHOGUN.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveredTool {
    pub id: &'static str,
    pub label: &'static str,
    pub setup_client: Option<&'static str>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDiscovery {
    pub tools: Vec<DiscoveredTool>,
    /// Some locations could not be inspected. Results found elsewhere remain valid.
    pub partial: bool,
}

struct ToolSpec {
    id: &'static str,
    label: &'static str,
    setup_client: Option<&'static str>,
    apps: &'static [&'static str],
    commands: &'static [&'static str],
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        id: "codex",
        label: "Codex",
        setup_client: Some("codex-cli"),
        apps: &["Codex.app"],
        commands: &["codex"],
    },
    ToolSpec {
        id: "claude-code",
        label: "Claude Code",
        setup_client: Some("claude-code"),
        apps: &["Claude Code.app"],
        commands: &["claude"],
    },
    ToolSpec {
        id: "claude-desktop",
        label: "Claude Desktop",
        setup_client: Some("claude-desktop"),
        apps: &["Claude.app", "Claude Desktop.app"],
        commands: &[],
    },
    ToolSpec {
        id: "cursor",
        label: "Cursor",
        setup_client: Some("cursor"),
        apps: &["Cursor.app"],
        commands: &["cursor"],
    },
    ToolSpec {
        id: "chatgpt-desktop",
        label: "ChatGPT desktop",
        setup_client: None,
        apps: &["ChatGPT.app"],
        commands: &[],
    },
    ToolSpec {
        id: "devin",
        label: "Devin",
        setup_client: None,
        apps: &["Devin.app"],
        commands: &["devin"],
    },
    ToolSpec {
        id: "windsurf",
        label: "Windsurf",
        setup_client: None,
        apps: &["Windsurf.app", "Codeium.app"],
        commands: &["windsurf"],
    },
    ToolSpec {
        id: "gemini",
        label: "Gemini",
        setup_client: None,
        apps: &["Gemini.app"],
        commands: &["gemini"],
    },
    ToolSpec {
        id: "vscode",
        label: "Visual Studio Code",
        setup_client: None,
        apps: &["Visual Studio Code.app"],
        commands: &["code"],
    },
    ToolSpec {
        id: "zed",
        label: "Zed",
        setup_client: None,
        apps: &["Zed.app"],
        commands: &["zed"],
    },
    ToolSpec {
        id: "opencode",
        label: "OpenCode",
        setup_client: None,
        apps: &["OpenCode.app", "OpenCode Desktop.app"],
        commands: &["opencode"],
    },
    ToolSpec {
        id: "aider",
        label: "Aider",
        setup_client: None,
        apps: &[],
        commands: &["aider"],
    },
    ToolSpec {
        id: "goose",
        label: "Goose",
        setup_client: None,
        apps: &["Goose.app"],
        commands: &["goose"],
    },
    ToolSpec {
        id: "amp",
        label: "Amp",
        setup_client: None,
        apps: &["Amp.app"],
        commands: &["amp"],
    },
    ToolSpec {
        id: "continue",
        label: "Continue",
        setup_client: None,
        apps: &["Continue.app"],
        commands: &[],
    },
    ToolSpec {
        id: "trae",
        label: "Trae",
        setup_client: None,
        apps: &["Trae.app"],
        commands: &["trae"],
    },
    ToolSpec {
        id: "kiro",
        label: "Kiro",
        setup_client: None,
        apps: &["Kiro.app"],
        commands: &["kiro"],
    },
    ToolSpec {
        id: "antigravity",
        label: "Antigravity",
        setup_client: None,
        apps: &["Antigravity.app"],
        commands: &["antigravity"],
    },
];

/// Scan standard app and executable locations without launching tools or opening their configs.
pub fn scan() -> ToolDiscovery {
    let home = crate::startup_report::home_dir();
    let mut app_roots = vec![PathBuf::from("/Applications")];
    let mut command_roots = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    if let Some(home) = &home {
        app_roots.push(home.join("Applications"));
        command_roots.extend(
            [
                ".local/bin",
                ".claude/local",
                ".npm-global/bin",
                ".bun/bin",
                ".cargo/bin",
                ".volta/bin",
                ".local/share/mise/shims",
                "Library/pnpm",
                ".local/share/pnpm",
            ]
            .map(|dir| home.join(dir)),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        command_roots.extend(std::env::split_paths(&path));
    }
    let mut result = scan_in(&app_roots, &command_roots);
    if home.is_none() {
        result.partial = true;
    }
    result
}

fn scan_in(app_roots: &[PathBuf], command_roots: &[PathBuf]) -> ToolDiscovery {
    let mut found = BTreeSet::new();
    let mut partial = false;
    for root in app_roots {
        scan_apps(root, 1, &mut found, &mut partial);
    }
    let mut checked = BTreeSet::new();
    for root in command_roots {
        if !checked.insert(root) {
            continue;
        }
        for tool in TOOLS {
            for command in tool.commands {
                match fs::metadata(root.join(command)) {
                    Ok(metadata) if executable(&metadata) => {
                        found.insert(tool.id);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => partial = true,
                }
            }
        }
    }
    ToolDiscovery {
        tools: TOOLS
            .iter()
            .filter(|tool| found.contains(tool.id))
            .map(|tool| DiscoveredTool {
                id: tool.id,
                label: tool.label,
                setup_client: tool.setup_client,
            })
            .collect(),
        partial,
    }
}

fn scan_apps(root: &Path, nested: usize, found: &mut BTreeSet<&'static str>, partial: &mut bool) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            *partial = true;
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *partial = true;
            continue;
        };
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                *partial = true;
                continue;
            }
        };
        if !metadata.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".app") {
            for tool in TOOLS {
                if tool.apps.iter().any(|app| name.eq_ignore_ascii_case(app))
                    && app_bundle_present(&path, partial)
                {
                    found.insert(tool.id);
                }
            }
        } else if nested > 0 {
            scan_apps(&path, nested - 1, found, partial);
        }
    }
}

fn app_bundle_present(path: &Path, partial: &mut bool) -> bool {
    let info = match fs::metadata(path.join("Contents/Info.plist")) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => {
            *partial = true;
            return false;
        }
    };
    if !info.is_file() {
        return false;
    }
    let entries = match fs::read_dir(path.join("Contents/MacOS")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => {
            *partial = true;
            return false;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *partial = true;
            continue;
        };
        match fs::metadata(entry.path()) {
            Ok(metadata) if executable(&metadata) => return true,
            Ok(_) => {}
            Err(_) => *partial = true,
        }
    }
    false
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn make_app(path: &Path) {
        fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        fs::write(path.join("Contents/Info.plist"), b"plist").unwrap();
        make_executable(&path.join("Contents/MacOS/launcher"));
    }

    #[test]
    #[cfg(unix)]
    fn scans_apps_and_cli_without_configs_or_paths() {
        let root = std::env::temp_dir().join(format!(
            "shogun-discovery-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let apps = root.join("Applications");
        let bin = root.join("bin");
        make_app(&apps.join("Vendor/Windsurf.app"));
        make_app(&apps.join("Codex.app"));
        fs::create_dir_all(apps.join("Cursor.app")).unwrap(); // Empty bundle is not installed.
        let chatgpt = apps.join("ChatGPT.app/Contents");
        fs::create_dir_all(chatgpt.join("MacOS")).unwrap();
        fs::write(chatgpt.join("Info.plist"), b"plist").unwrap();
        fs::write(chatgpt.join("MacOS/launcher"), b"not executable").unwrap();
        fs::create_dir_all(&bin).unwrap();
        make_executable(&bin.join("codex")); // app + CLI becomes one entry.
        make_executable(&bin.join("claude"));
        fs::write(bin.join("aider"), b"not executable").unwrap();
        fs::write(root.join("stale-config.toml"), b"[mcp_servers.cursor]").unwrap();
        let result = scan_in(&[apps], &[bin]);
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(
            result.tools.iter().map(|tool| tool.id).collect::<Vec<_>>(),
            ["codex", "claude-code", "windsurf"]
        );
        assert_eq!(result.tools[0].setup_client, Some("codex-cli"));
        assert!(!result.partial);
        assert!(!json.contains(root.to_str().unwrap()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_location_keeps_partial_results() {
        let root =
            std::env::temp_dir().join(format!("shogun-discovery-error-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        make_app(&root.join("Codex.app"));
        let broken = root.join("not-a-directory");
        fs::write(&broken, b"").unwrap();
        let result = scan_in(&[root.clone(), broken], &[]);
        assert!(result.partial);
        #[cfg(unix)]
        assert_eq!(result.tools[0].id, "codex");
        fs::remove_dir_all(root).unwrap();
    }
}
