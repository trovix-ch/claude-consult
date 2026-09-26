//! The terminal, entered and always left again.

use std::io;

use ratatui::{DefaultTerminal, Frame};

/// Raw mode and the alternate screen for as long as this lives.
///
/// Built on `ratatui::try_init`, which also installs a panic hook that restores the
/// terminal before the panic message prints; dropping the guard restores it on every
/// other way out, `?` included. Install any panic hook of your own before
/// [`Terminal::enter`], or it runs on a terminal still in raw mode.
pub struct Terminal {
    inner: DefaultTerminal,
}

impl Terminal {
    /// Enters raw mode and the alternate screen.
    pub fn enter() -> io::Result<Self> {
        Ok(Self {
            inner: ratatui::try_init()?,
        })
    }

    /// Draws one frame.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.inner.draw(render).map(|_| ())
    }

    /// The underlying ratatui terminal.
    pub fn inner(&mut self) -> &mut DefaultTerminal {
        &mut self.inner
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Nothing useful can be done with a failure here; the shell would show it.
        let _ = ratatui::try_restore();
    }
}
