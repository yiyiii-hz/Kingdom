#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub title: String,
    pub body: String,
    pub options: Vec<PopupOption>,
    pub timeout_secs: Option<u32>,
    pub default_on_timeout: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupOption {
    pub label: String,
    pub key: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupResult {
    Selected(usize),
    Timeout,
    Dismissed,
}
