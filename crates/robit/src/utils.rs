use std::env;
use std::path::{Path, PathBuf};

pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" || input.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            let mut path = PathBuf::from(home);
            if input.len() > 2 {
                path.push(&input[2..]);
            }
            return path;
        }
    }
    PathBuf::from(input)
}

pub fn clean_path(path: &Path) -> PathBuf {
    if path.exists() {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}
