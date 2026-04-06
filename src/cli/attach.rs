use crate::tmux::TmuxController;

pub fn run_attach(session_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let controller = TmuxController::new(session_name);
    if !controller.session_exists() {
        eprintln!("kingdom: no active session '{}'", session_name);
        std::process::exit(1);
    }
    controller.attach()?;
    Ok(())
}
