use crate::tmux::popup::{Popup, PopupResult};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, TmuxError>;

#[derive(Debug)]
pub enum TmuxError {
    Io(std::io::Error),
    CommandFailed(String),
}

impl Display for TmuxError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::CommandFailed(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for TmuxError {}

impl From<std::io::Error> for TmuxError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxResponse {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait TmuxOps: Send + Sync {
    fn run(&self, args: &[String]) -> std::io::Result<TmuxResponse>;
    fn exec(&self, args: &[String]) -> std::io::Result<()>;
}

#[derive(Default)]
struct RealTmux;

impl TmuxOps for RealTmux {
    fn run(&self, args: &[String]) -> std::io::Result<TmuxResponse> {
        let output = Command::new("tmux").args(args).output()?;
        Ok(TmuxResponse {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }

    fn exec(&self, args: &[String]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            let error = Command::new("tmux").args(args).exec();
            Err(error)
        }
        #[cfg(not(unix))]
        {
            let status = Command::new("tmux").args(args).status()?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "tmux attach-session failed",
                ))
            }
        }
    }
}

#[derive(Clone)]
pub struct TmuxController {
    pub session_name: String,
    ops: Arc<dyn TmuxOps>,
}

impl TmuxController {
    pub fn new(session_name: impl Into<String>) -> Self {
        Self {
            session_name: session_name.into(),
            ops: Arc::new(RealTmux),
        }
    }

    pub fn with_ops(session_name: impl Into<String>, ops: Arc<dyn TmuxOps>) -> Self {
        Self {
            session_name: session_name.into(),
            ops,
        }
    }

    pub fn update_status_bar(&self, rendered: &str) -> Result<()> {
        let set = self.ops.run(&[
            "set-option".to_string(),
            "-t".to_string(),
            self.session_name.clone(),
            "-g".to_string(),
            "status-right".to_string(),
            rendered.to_string(),
        ]);
        let Ok(set) = set else {
            return Ok(());
        };
        if !set.success {
            return Ok(());
        }
        let _ = self
            .ops
            .run(&["refresh-client".to_string(), "-S".to_string()]);
        Ok(())
    }

    pub fn show_popup(&self, popup: &Popup) -> Result<PopupResult> {
        if !self.supports_display_popup_inner() {
            self.inject_popup_fallback(popup);
            return Ok(PopupResult::Dismissed);
        }

        let script_path = popup_script_path("sh");
        let result_path = popup_script_path("result");
        std::fs::write(
            &script_path,
            render_popup_script(popup, result_path.as_os_str().to_string_lossy().as_ref()),
        )?;

        let response = self.ops.run(&[
            "display-popup".to_string(),
            "-E".to_string(),
            script_path.as_os_str().to_string_lossy().to_string(),
        ]);

        let popup_result = match response {
            Ok(response) if response.success => read_popup_result(&result_path)?,
            _ => {
                self.inject_popup_fallback(popup);
                PopupResult::Dismissed
            }
        };

        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&result_path);
        Ok(popup_result)
    }

    pub fn inject_line(&self, pane_id: &str, line: &str) -> Result<()> {
        let literal = self.ops.run(&[
            "send-keys".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
            "-l".to_string(),
            line.to_string(),
        ]);
        let enter = self.ops.run(&[
            "send-keys".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
            "Enter".to_string(),
        ]);

        match (literal, enter) {
            (Ok(first), Ok(second)) if first.success && second.success => Ok(()),
            _ => {
                tracing::warn!(pane_id, "tmux inject_line failed; skipping");
                Ok(())
            }
        }
    }

    pub fn respawn_pane(&self, pane_id: &str, command: &str) -> Result<()> {
        let response = self.ops.run(&[
            "respawn-pane".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
            "-k".to_string(),
            command.to_string(),
        ])?;
        if response.success {
            Ok(())
        } else {
            Err(TmuxError::CommandFailed(response.stderr))
        }
    }

    pub fn create_session(&self, initial_command: Option<&str>) -> Result<()> {
        let mut args = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            self.session_name.clone(),
        ];
        if let Some(command) = initial_command {
            args.push(command.to_string());
        }
        let response = self.ops.run(&args)?;
        if response.success {
            Ok(())
        } else {
            Err(TmuxError::CommandFailed(response.stderr))
        }
    }

    pub fn session_exists(&self) -> bool {
        self.ops
            .run(&[
                "has-session".to_string(),
                "-t".to_string(),
                self.session_name.clone(),
            ])
            .map(|response| response.success)
            .unwrap_or(false)
    }

    pub fn attach(&self) -> Result<()> {
        self.ops
            .exec(&[
                "attach-session".to_string(),
                "-t".to_string(),
                self.session_name.clone(),
            ])
            .map_err(TmuxError::Io)
    }

    pub fn tmux_version() -> Option<(u32, u32)> {
        parse_tmux_version(
            &RealTmux
                .run(&["-V".to_string()])
                .ok()?
                .stdout,
        )
    }

    pub fn supports_display_popup() -> bool {
        matches!(Self::tmux_version(), Some((major, minor)) if major > 3 || (major == 3 && minor >= 2))
    }

    fn supports_display_popup_inner(&self) -> bool {
        match self.ops.run(&["-V".to_string()]) {
            Ok(response) => matches!(parse_tmux_version(&response.stdout), Some((major, minor)) if major > 3 || (major == 3 && minor >= 2)),
            Err(_) => false,
        }
    }

    fn inject_popup_fallback(&self, popup: &Popup) {
        let options = popup
            .options
            .iter()
            .map(|option| format!("{} [{}]", option.label, option.key))
            .collect::<Vec<_>>()
            .join(" | ");
        let body = format!(
            "[Kingdom 需要确认] {}\n  {}\n  操作: {}",
            popup.title, popup.body, options
        );
        let fallback_target = format!("{}:0.0", self.session_name);
        let _ = self.inject_line(&fallback_target, &body);
    }
}

fn popup_script_path(ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kingdom_popup_{}.{}", uuid::Uuid::new_v4().simple(), ext))
}

fn render_popup_script(popup: &Popup, result_path: &str) -> String {
    let mut script = String::from("#!/bin/sh\nset -eu\nclear\n");
    script.push_str(&format!("printf '%s\\n' {}\n", sh_quote(&popup.title)));
    script.push_str("printf '\\n'\n");
    script.push_str(&format!("printf '%s\\n' {}\n", sh_quote(&popup.body)));
    script.push_str("printf '\\n'\n");
    for (index, option) in popup.options.iter().enumerate() {
        script.push_str(&format!(
            "printf '%s\\n' {}\n",
            sh_quote(&format!("{} [{}] -> {}", option.label, option.key, index))
        ));
    }
    script.push_str("printf '\\n'\n");
    if let Some(timeout) = popup.timeout_secs {
        script.push_str(&format!("read -r -n 1 -t {timeout} choice || true\n"));
    } else {
        script.push_str("read -r -n 1 choice || true\n");
    }
    script.push_str("choice=${choice:-}\n");
    script.push_str("result='dismissed'\n");
    for (index, option) in popup.options.iter().enumerate() {
        let key = option.key.to_string();
        script.push_str(&format!(
            "[ \"$choice\" = {} ] && result={} \n",
            sh_quote(&key),
            sh_quote(&index.to_string())
        ));
    }
    if let Some(default_index) = popup.default_on_timeout {
        script.push_str(&format!(
            "[ -z \"$choice\" ] && result={} \n",
            sh_quote(&format!("timeout:{default_index}"))
        ));
    } else {
        script.push_str("[ -z \"$choice\" ] && result='timeout'\n");
    }
    script.push_str(&format!("printf '%s' \"$result\" > {}\n", sh_quote(result_path)));
    script
}

fn read_popup_result(path: &PathBuf) -> Result<PopupResult> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(PopupResult::Dismissed),
        Err(error) => return Err(TmuxError::Io(error)),
    };
    let trimmed = raw.trim();
    if let Some(index) = trimmed.strip_prefix("timeout:") {
        return Ok(index
            .parse::<usize>()
            .map(PopupResult::Selected)
            .unwrap_or(PopupResult::Timeout));
    }
    if trimmed == "timeout" {
        return Ok(PopupResult::Timeout);
    }
    if trimmed == "dismissed" || trimmed.is_empty() {
        return Ok(PopupResult::Dismissed);
    }
    Ok(trimmed
        .parse::<usize>()
        .map(PopupResult::Selected)
        .unwrap_or(PopupResult::Dismissed))
}

fn parse_tmux_version(raw: &str) -> Option<(u32, u32)> {
    let version = raw.strip_prefix("tmux ")?.trim();
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor_raw = parts.next().unwrap_or("0");
    let minor = minor_raw
        .chars()
        .take_while(|char| char.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

fn sh_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockTmux {
        responses: Mutex<HashMap<String, VecDeque<std::io::Result<TmuxResponse>>>>,
        exec_response: Mutex<Option<std::io::Result<()>>>,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockTmux {
        pub fn push_response(&self, command: &str, response: std::io::Result<TmuxResponse>) {
            self.responses
                .lock()
                .unwrap()
                .entry(command.to_string())
                .or_default()
                .push_back(response);
        }

        #[allow(dead_code)]
        pub fn set_exec_response(&self, response: std::io::Result<()>) {
            *self.exec_response.lock().unwrap() = Some(response);
        }
    }

    impl TmuxOps for MockTmux {
        fn run(&self, args: &[String]) -> std::io::Result<TmuxResponse> {
            self.calls.lock().unwrap().push(args.to_vec());
            let key = args.first().cloned().unwrap_or_default();
            self.responses
                .lock()
                .unwrap()
                .get_mut(&key)
                .and_then(|queue| queue.pop_front())
                .unwrap_or(Ok(TmuxResponse {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }

        fn exec(&self, args: &[String]) -> std::io::Result<()> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.exec_response.lock().unwrap().take().unwrap_or(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testsupport::MockTmux;
    use super::*;
    use crate::tmux::popup::{Popup, PopupOption};

    fn controller(mock: Arc<MockTmux>) -> TmuxController {
        TmuxController::with_ops("kingdom", mock)
    }

    #[test]
    fn update_status_bar_swallows_tmux_failures() {
        let mock = Arc::new(MockTmux::default());
        mock.push_response(
            "set-option",
            Ok(TmuxResponse {
                success: false,
                stdout: String::new(),
                stderr: "boom".to_string(),
            }),
        );
        let controller = controller(mock);
        assert!(controller.update_status_bar("status").is_ok());
    }

    #[test]
    fn inject_line_swallows_tmux_failures() {
        let mock = Arc::new(MockTmux::default());
        mock.push_response(
            "send-keys",
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
        );
        let controller = controller(mock);
        assert!(controller.inject_line("%1", "hello").is_ok());
    }

    #[test]
    fn popup_falls_back_when_popup_not_supported() {
        let mock = Arc::new(MockTmux::default());
        mock.push_response(
            "-V",
            Ok(TmuxResponse {
                success: true,
                stdout: "tmux 3.1c".to_string(),
                stderr: String::new(),
            }),
        );
        let controller = controller(Arc::clone(&mock));
        let popup = Popup {
            title: "Confirm".to_string(),
            body: "Proceed?".to_string(),
            options: vec![PopupOption {
                label: "Yes".to_string(),
                key: 'y',
            }],
            timeout_secs: None,
            default_on_timeout: None,
        };
        assert_eq!(controller.show_popup(&popup).unwrap(), PopupResult::Dismissed);
        let calls = mock.calls.lock().unwrap();
        assert!(calls.iter().any(|args| args.first().map(|arg| arg == "send-keys").unwrap_or(false)));
    }

    #[test]
    fn session_exists_returns_false_on_failure() {
        let mock = Arc::new(MockTmux::default());
        mock.push_response(
            "has-session",
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
        );
        assert!(!controller(mock).session_exists());
    }

    #[test]
    fn supports_display_popup_parses_alpha_versions() {
        assert_eq!(parse_tmux_version("tmux 3.3a"), Some((3, 3)));
    }
}
