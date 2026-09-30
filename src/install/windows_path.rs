//! Pure Windows PATH policy. The original OsString is preserved verbatim.
use std::ffi::{OsStr, OsString};

fn normalize(entry: &str, local_app_data: &str) -> String {
    let mut entry = entry
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase();
    entry = entry.replace("%localappdata%", &local_app_data.to_lowercase());
    entry
        .trim_start_matches("\\\\?\\")
        .trim_end_matches('\\')
        .to_owned()
}

pub(super) fn append_path(
    existing: &OsStr,
    bin: &OsStr,
    local_app_data: &OsStr,
) -> Option<OsString> {
    let target = normalize(&bin.to_string_lossy(), &local_app_data.to_string_lossy());
    if existing
        .to_string_lossy()
        .split(';')
        .any(|entry| normalize(entry, &local_app_data.to_string_lossy()) == target)
    {
        return None;
    }
    let mut updated = existing.to_owned();
    if !existing.is_empty() && !existing.to_string_lossy().ends_with(';') {
        updated.push(";");
    }
    updated.push(bin);
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_path_empty_existing_case_expansion_and_separator() {
        let local = OsStr::new(r"C:\Users\Pedro\AppData\Local");
        let bin = OsStr::new(r"C:\Users\Pedro\AppData\Local\CodexGuard\bin");
        assert_eq!(append_path(OsStr::new(""), bin, local).unwrap(), bin);
        for existing in [r"C:\Tools", r"C:\Tools;"] {
            assert_eq!(
                append_path(OsStr::new(existing), bin, local).unwrap(),
                OsString::from(format!("C:\\Tools;{}", bin.to_string_lossy()))
            );
        }
        for existing in [
            bin.to_string_lossy().into_owned(),
            r"c:\users\pedro\appdata\local\codexguard\BIN\;C:\Tools".into(),
            r"%LOCALAPPDATA%\CodexGuard\bin".into(),
            r#""C:\Users\Pedro\AppData\Local\CodexGuard\bin";"#.into(),
        ] {
            assert!(append_path(OsStr::new(&existing), bin, local).is_none());
        }
        let long = "C:\\Long;".repeat(10000);
        let updated = append_path(OsStr::new(&long), bin, local).unwrap();
        assert!(updated.to_string_lossy().starts_with(&long));
        assert!(append_path(&updated, bin, local).is_none());
    }
}
