//! Stable machine-readable diagnostics. Messages are for display, never branching.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    Unsupported,
    NotExposed,
    PermissionDenied,
    ReadFailed,
    InvalidData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOperation {
    Read,
    Enumerate,
    Inspect,
    Decode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    /// Kernel device/channel ID, not a localized display name.
    pub device: Option<String>,
    /// Absolute path in the observed machine, independent of the fixture root.
    pub path: PathBuf,
    pub operation: DiagnosticOperation,
    /// Original OS error when available; decode errors have no errno.
    pub errno: Option<i32>,
    pub message: String,
}

impl Diagnostic {
    pub fn from_io(
        device: Option<&str>,
        path: &Path,
        operation: DiagnosticOperation,
        error: &io::Error,
    ) -> Self {
        let code = match error.kind() {
            io::ErrorKind::NotFound => DiagnosticCode::NotExposed,
            io::ErrorKind::PermissionDenied => DiagnosticCode::PermissionDenied,
            io::ErrorKind::Unsupported => DiagnosticCode::Unsupported,
            io::ErrorKind::InvalidData => DiagnosticCode::InvalidData,
            io::ErrorKind::InvalidInput if operation == DiagnosticOperation::Decode => {
                DiagnosticCode::InvalidData
            }
            _ => DiagnosticCode::ReadFailed,
        };
        Self {
            code,
            device: device.map(str::to_owned),
            path: path.into(),
            operation,
            errno: error.raw_os_error(),
            message: error.to_string(),
        }
    }
}

pub(crate) struct Reader<'a> {
    pub root: &'a Path,
    pub diagnostics: Vec<Diagnostic>,
}

impl<'a> Reader<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            diagnostics: Vec::new(),
        }
    }
    pub fn issue(
        &mut self,
        device: &str,
        path: &str,
        operation: DiagnosticOperation,
        code: DiagnosticCode,
        message: &str,
    ) {
        self.diagnostics.push(Diagnostic {
            code,
            device: Some(device.into()),
            path: format!("/{path}").into(),
            operation,
            errno: None,
            message: message.into(),
        });
    }
    fn io_error(
        &mut self,
        device: &str,
        path: &str,
        operation: DiagnosticOperation,
        error: io::Error,
    ) {
        let mut diagnostic = Diagnostic::from_io(
            Some(device),
            Path::new(&format!("/{path}")),
            operation,
            &error,
        );
        if self.root != Path::new("/") {
            diagnostic.message = diagnostic
                .message
                .replace(self.root.to_string_lossy().as_ref(), "");
        }
        self.diagnostics.push(diagnostic);
    }
    /// Real non-Linux host probes are unsupported; injected Linux trees still work.
    pub fn supported(&mut self, device: &str, path: &str) -> bool {
        if self.root == Path::new("/") && !cfg!(target_os = "linux") {
            self.issue(
                device,
                path,
                DiagnosticOperation::Inspect,
                DiagnosticCode::Unsupported,
                "This observation requires Linux sysfs",
            );
            false
        } else {
            true
        }
    }
    pub fn bytes(&mut self, device: &str, path: &str) -> Option<Vec<u8>> {
        match crate::resolve_path_in_root(self.root, path).and_then(fs::read) {
            Ok(value) => Some(value),
            Err(error) => {
                self.io_error(device, path, DiagnosticOperation::Read, error);
                None
            }
        }
    }
    pub fn text(&mut self, device: &str, path: &str) -> Option<String> {
        let bytes = self.bytes(device, path)?;
        match String::from_utf8(bytes) {
            Ok(value) if !value.trim().is_empty() && !value.contains('\0') => {
                Some(value.trim().into())
            }
            _ => {
                self.invalid(device, path, "Expected non-empty UTF-8 text without NUL");
                None
            }
        }
    }
    pub fn parsed<T>(
        &mut self,
        device: &str,
        path: &str,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Option<T> {
        let text = self.text(device, path)?;
        match parse(&text) {
            Some(value) => Some(value),
            None => {
                self.invalid(device, path, "Invalid attribute value");
                None
            }
        }
    }
    pub fn number<T: std::str::FromStr>(&mut self, device: &str, path: &str) -> Option<T> {
        self.parsed(device, path, |text| text.parse().ok())
    }
    pub fn boolean(&mut self, device: &str, path: &str) -> Option<bool> {
        self.parsed(device, path, |text| match text {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        })
    }
    pub fn invalid(&mut self, device: &str, path: &str, message: &str) {
        self.issue(
            device,
            path,
            DiagnosticOperation::Decode,
            DiagnosticCode::InvalidData,
            message,
        );
    }
    pub fn entries(&mut self, device: &str, path: &str) -> Vec<String> {
        match crate::resolve_path_in_root(self.root, path).and_then(fs::read_dir) {
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    match entry {
                        Ok(entry) => match entry.file_name().into_string() {
                            Ok(name) => names.push(name),
                            Err(_) => self.invalid(device, path, "Non-UTF-8 sysfs entry name"),
                        },
                        Err(error) => {
                            self.io_error(device, path, DiagnosticOperation::Enumerate, error)
                        }
                    }
                }
                names.sort();
                names
            }
            Err(error) => {
                self.io_error(device, path, DiagnosticOperation::Enumerate, error);
                Vec::new()
            }
        }
    }
    /// None means inspection failed; false means not exposed, never proof of absence.
    pub fn directory(&mut self, device: &str, path: &str) -> Option<bool> {
        match crate::resolve_path_in_root(self.root, path).and_then(fs::metadata) {
            Ok(metadata) if metadata.is_dir() => Some(true),
            Ok(_) => {
                self.invalid(device, path, "Expected a sysfs directory");
                None
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.io_error(device, path, DiagnosticOperation::Inspect, error);
                Some(false)
            }
            Err(error) => {
                self.io_error(device, path, DiagnosticOperation::Inspect, error);
                None
            }
        }
    }
}

pub(crate) fn indexed(name: &str, prefix: &str) -> Option<u32> {
    let tail = name.strip_prefix(prefix)?;
    (!tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
        .then(|| tail.parse().ok())
        .flatten()
}
