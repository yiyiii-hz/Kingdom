use std::fmt::{Display, Formatter};
use std::io::Write;

pub type Result<T> = std::result::Result<T, NotificationError>;

#[derive(Debug)]
pub enum NotificationError {
    Io(std::io::Error),
}

impl Display for NotificationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NotificationError {}

impl From<std::io::Error> for NotificationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    None,
    Bell,
    System,
}

pub fn send_notification(title: &str, body: &str, level: NotificationLevel) -> Result<()> {
    match level {
        NotificationLevel::None => {}
        NotificationLevel::Bell => {
            print!("\x07");
            std::io::stdout().flush()?;
        }
        NotificationLevel::System => {
            #[cfg(target_os = "macos")]
            {
                let script = format!(
                    "display notification {:?} with title {:?}",
                    body, title
                );
                std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output()?;
            }
            #[cfg(target_os = "linux")]
            {
                std::process::Command::new("notify-send")
                    .args([title, body])
                    .output()?;
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                print!("\x07");
                std::io::stdout().flush()?;
            }
        }
    }
    Ok(())
}
