pub mod layout;
pub mod manager;

use std::cell::RefCell;

use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

use crate::event::{CommandHistory, EventProxy, InlineSearchState, SearchState, SuggestionState};
/// A single terminal pane within a window.
pub struct Pane {
    pub terminal: std::sync::Arc<FairMutex<Term<EventProxy>>>,
    pub notifier: Notifier,
    pub search_state: SearchState,
    pub inline_search_state: InlineSearchState,
    pub suggestion_state: SuggestionState,
    pub command_history: RefCell<CommandHistory>,
    #[cfg(not(windows))]
    pub master_fd: i32,
    #[cfg(not(windows))]
    pub shell_pid: u32,
}
