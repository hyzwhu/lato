use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, panic, sync::Once};

pub type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    pub fn enter() -> Result<(Self, TuiTerminal), String> {
        install_panic_restore();
        enable_raw_mode().map_err(|error| format!("enable terminal raw mode: {error}"))?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        ) {
            let _ = disable_raw_mode();
            return Err(format!("enter alternate screen: {error}"));
        }
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).map_err(|error| {
            restore();
            format!("initialize terminal: {error}")
        })?;
        Ok((Self { active: true }, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            restore();
            self.active = false;
        }
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
}

fn install_panic_restore() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
