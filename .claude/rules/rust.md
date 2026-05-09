---
description: Rust coding conventions for hvecuum
globs: "**/*.rs"
---

# Rust Conventions

- Use `anyhow` for error handling in the binary, `thiserror` for library error types
- Use `clap` derive API for CLI argument parsing
- Use `serde` + `serde_yaml` for config file parsing
- Use `std::process::Command` to invoke ffmpeg/ffprobe
- Parse ffprobe output as JSON using `serde_json`
- Tests go in the same file as the code they test (inline `#[cfg(test)]` modules)
- Prefer `walkdir` for directory traversal
- Keep functions small and focused
- Log with `log` + `env_logger` crate
