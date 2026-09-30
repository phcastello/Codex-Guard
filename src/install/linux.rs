use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use {
    super::{InstallPaths, PathOutcome},
    anyhow::{Context, Result},
    std::{env, fs},
};

const MARKER: &str = "# Codex Guard user PATH";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

fn shell_kind(shell: &Path) -> Option<Shell> {
    match shell.file_name()?.to_str()?.trim_start_matches('-') {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        _ => None,
    }
}

fn config_path(home: &Path, shell: Shell, zdotdir: Option<&Path>, xdg: Option<&Path>) -> PathBuf {
    match shell {
        Shell::Bash => home.join(".bashrc"),
        Shell::Zsh => zdotdir
            .filter(|p| p.is_absolute())
            .unwrap_or(home)
            .join(".zshrc"),
        Shell::Fish => xdg
            .filter(|p| p.is_absolute())
            .map(Path::to_owned)
            .unwrap_or_else(|| home.join(".config"))
            .join("fish/config.fish"),
    }
}

fn has_path(path: &OsStr, bin: &Path) -> bool {
    std::env::split_paths(path).any(|entry| {
        entry == bin
            || std::fs::canonicalize(&entry)
                .ok()
                .zip(std::fs::canonicalize(bin).ok())
                .is_some_and(|(entry, bin)| entry == bin)
    })
}

// No shell commands are executed. This computes an idempotent config edit.
fn updated_config(existing: &str, shell: Shell) -> Option<String> {
    let configured = existing.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        let line = line.split('#').next().unwrap_or("");
        let local_bin = ["$HOME/.local/bin", "${HOME}/.local/bin", "~/.local/bin"]
            .iter()
            .any(|path| {
                line.match_indices(path).any(|(at, _)| {
                    line[at + path.len()..].chars().next().is_none_or(|next| {
                        next.is_whitespace() || matches!(next, ':' | '"' | '\'' | ';' | ')')
                    })
                })
            });
        local_bin
            && match shell {
                Shell::Bash | Shell::Zsh => line.contains("PATH="),
                Shell::Fish => {
                    line.contains("fish_add_path")
                        || (line.starts_with("set ") && line.contains("PATH"))
                }
            }
    });
    if configured {
        return None;
    }
    let block = match shell {
        Shell::Bash | Shell::Zsh => "case \":$PATH:\" in\n  *\":$HOME/.local/bin:\"*) ;;\n  *) export PATH=\"$HOME/.local/bin:$PATH\" ;;\nesac\n",
        Shell::Fish => "fish_add_path --path \"$HOME/.local/bin\"\n",
    };
    Some(format!(
        "{existing}{}{MARKER}\n{block}",
        if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        }
    ))
}

#[cfg(unix)]
pub(super) fn install_alias(paths: &InstallPaths) -> Result<()> {
    use std::os::unix::fs::symlink;
    match fs::symlink_metadata(&paths.alias) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(&paths.alias)? == Path::new("codex-guard") =>
        {
            return Ok(())
        }
        Ok(_) => anyhow::bail!(
            "{} already exists and is not a Codex Guard symlink; it was preserved",
            paths.alias.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    symlink("codex-guard", &paths.alias).context("create cg symlink")
}

#[cfg(unix)]
fn write_shell_config(path: &Path, shell: Shell) -> Result<bool> {
    use std::io::Write;
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let Some(updated) = updated_config(&existing, shell) else {
        return Ok(false);
    };
    fs::create_dir_all(path.parent().context("shell config parent unavailable")?)?;
    // Append only our addition, preserving the user's file and its permissions.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(updated[existing.len()..].as_bytes())?;
    Ok(true)
}

#[cfg(unix)]
pub(super) fn ensure_user_path(bin: &Path) -> Result<PathOutcome> {
    let current_path = env::var_os("PATH").unwrap_or_default();
    let home = PathBuf::from(env::var_os("HOME").context("HOME unavailable")?);
    let shell = env::var_os("SHELL").map(PathBuf::from);
    let zdotdir = env::var_os("ZDOTDIR").map(PathBuf::from);
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    ensure_path_for(
        bin,
        &home,
        &current_path,
        shell.as_deref(),
        zdotdir.as_deref(),
        xdg.as_deref(),
    )
}

#[cfg(unix)]
fn ensure_path_for(
    bin: &Path,
    home: &Path,
    current_path: &OsStr,
    shell: Option<&Path>,
    zdotdir: Option<&Path>,
    xdg: Option<&Path>,
) -> Result<PathOutcome> {
    if has_path(current_path, bin) {
        return Ok(PathOutcome::Ready(String::new()));
    }
    let Some(shell) = shell.and_then(shell_kind) else {
        return Ok(PathOutcome::Manual(format!("Shell not recognized. Add {} to PATH to use codex-guard and cg in new terminals. This execution continues normally.", bin.display())));
    };
    let path = config_path(home, shell, zdotdir, xdg);
    if write_shell_config(&path, shell)? {
        Ok(PathOutcome::Ready(format!(
            "Added user PATH configuration to {}. Open a new terminal to use cg.",
            path.display()
        )))
    } else {
        Ok(PathOutcome::Ready(String::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_detection_and_config_locations() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        for (name, shell, config) in [
            ("bash", Shell::Bash, ".bashrc"),
            ("zsh", Shell::Zsh, ".zshrc"),
            ("fish", Shell::Fish, ".config/fish/config.fish"),
        ] {
            assert_eq!(shell_kind(Path::new(name)), Some(shell));
            assert_eq!(config_path(home, shell, None, None), home.join(config));
        }
        assert_eq!(shell_kind(Path::new("/bin/dash")), None);
        assert_eq!(shell_kind(Path::new("")), None);
        assert_eq!(
            config_path(home, Shell::Zsh, Some(home), None),
            home.join(".zshrc")
        );
        assert_eq!(
            config_path(home, Shell::Fish, None, Some(home)),
            home.join("fish/config.fish")
        );
    }

    #[test]
    fn shell_edits_are_idempotent_and_preserve_existing_content() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            for existing in ["", "# existing config", "# $HOME/.local/bin\n"] {
                let edited = updated_config(existing, shell).unwrap();
                assert!(edited.starts_with(existing));
                assert!(updated_config(&edited, shell).is_none());
                if shell == Shell::Fish {
                    assert!(edited.contains("fish_add_path --path"));
                    assert!(!edited.contains("export PATH"));
                } else {
                    assert!(edited.contains("case \":$PATH:\""));
                }
            }
        }
        for shell in [Shell::Bash, Shell::Zsh] {
            assert!(updated_config("export PATH=\"$HOME/.local/bin:$PATH\"\n", shell).is_none());
            assert!(updated_config("PATH=${HOME}/.local/bin:$PATH\n", shell).is_none());
            assert!(
                updated_config("export PATH=\"$HOME/.local/bin-other:$PATH\"\n", shell).is_some()
            );
        }
        assert!(updated_config("fish_add_path ~/.local/bin\n", Shell::Fish).is_none());
    }

    #[test]
    fn path_presence_is_exact_and_handles_trailing_slash() {
        let bin = Path::new("home/.local/bin");
        let path =
            std::env::join_paths([Path::new("other"), Path::new("home/.local/bin/")]).unwrap();
        assert!(has_path(&path, bin));
        assert!(!has_path(OsStr::new(""), bin));
        assert!(!has_path(OsStr::new("home/.local/bin-extra"), bin));
    }

    #[cfg(unix)]
    #[test]
    fn persistent_path_uses_only_injected_home_and_unknown_shell_keeps_running() {
        let temp = tempfile::tempdir().unwrap();
        let paths = InstallPaths::linux(temp.path());
        let present = std::env::join_paths([&paths.bin_dir]).unwrap();
        assert!(
            matches!(ensure_path_for(&paths.bin_dir, temp.path(), &present, None, None, None).unwrap(), PathOutcome::Ready(message) if message.is_empty())
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        assert!(matches!(
            ensure_path_for(
                &paths.bin_dir,
                temp.path(),
                OsStr::new(""),
                Some(Path::new("/bin/dash")),
                None,
                None
            )
            .unwrap(),
            PathOutcome::Manual(_)
        ));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        for shell in ["bash", "zsh", "fish"] {
            let result = ensure_path_for(
                &paths.bin_dir,
                temp.path(),
                OsStr::new(""),
                Some(Path::new(shell)),
                None,
                None,
            )
            .unwrap();
            assert!(matches!(result, PathOutcome::Ready(message) if !message.is_empty()));
            let result = ensure_path_for(
                &paths.bin_dir,
                temp.path(),
                OsStr::new(""),
                Some(Path::new(shell)),
                None,
                None,
            )
            .unwrap();
            assert!(matches!(result, PathOutcome::Ready(message) if message.is_empty()));
        }
        let bad_home = temp.path().join("not-a-directory");
        fs::write(&bad_home, b"preserve this file").unwrap();
        assert!(ensure_path_for(
            &paths.bin_dir,
            &bad_home,
            OsStr::new(""),
            Some(Path::new("bash")),
            None,
            None
        )
        .is_err());
        assert_eq!(fs::read(&bad_home).unwrap(), b"preserve this file");
    }

    #[cfg(unix)]
    #[test]
    fn only_mock_home_is_edited_and_alias_conflicts_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let paths = InstallPaths::linux(temp.path());
        fs::create_dir_all(&paths.bin_dir).unwrap();
        install_alias(&paths).unwrap();
        install_alias(&paths).unwrap();
        assert_eq!(
            fs::read_link(&paths.alias).unwrap(),
            Path::new("codex-guard")
        );
        fs::remove_file(&paths.alias).unwrap();
        fs::write(&paths.alias, b"unrelated cg").unwrap();
        assert!(install_alias(&paths).is_err());
        assert_eq!(fs::read(&paths.alias).unwrap(), b"unrelated cg");
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let config = config_path(temp.path(), shell, None, None);
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(&config, "# preserve me\n").unwrap();
            assert!(write_shell_config(&config, shell).unwrap());
            assert!(!write_shell_config(&config, shell).unwrap());
            assert_eq!(
                fs::read_to_string(&config).unwrap().matches(MARKER).count(),
                1
            );
        }
    }
}
