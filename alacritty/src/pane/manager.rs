use std::collections::HashMap;
use std::error::Error;
#[cfg(not(windows))]
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;

use log::info;
use winit::event_loop::EventLoopProxy;

use alacritty_terminal::event::OnResize;
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;

use crate::cli::WindowOptions;
use crate::config::UiConfig;
use crate::event::{Event, EventProxy, InlineSearchState, SearchState, SuggestionState};
use crate::pane::layout::{PaneBounds, PaneId, PaneLayout, SplitDirection};

use super::Pane;

/// Type alias for the pane creation return type.
type PaneTerminal = (Arc<FairMutex<Term<EventProxy>>>, Notifier, i32, u32);

/// Simpler dimensions for terminal resizing.
struct TermDimensions {
    columns: usize,
    lines: usize,
}

impl TermDimensions {
    fn new(columns: usize, lines: usize) -> Self {
        Self { columns, lines }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Manages all panes within a single window.
pub struct PaneManager {
    /// Binary tree layout of panes.
    layout: PaneLayout,
    /// All panes keyed by ID.
    panes: HashMap<PaneId, Pane>,
    /// ID of the currently focused pane.
    active_pane: PaneId,
    /// Next pane ID to assign.
    next_id: u64,
    /// Window-level bounds (computed from window size).
    window_bounds: PaneBounds,
    /// Pane bounds cache (computed from layout).
    pane_bounds: HashMap<PaneId, PaneBounds>,
    /// Border width for layout.
    border_width: f32,
    /// Minimum pane dimensions in pixels (width, height).
    min_pane_px: (f32, f32),
    /// Config reference.
    config: Rc<UiConfig>,
    /// Focus switch queued during event processing (applied after lock is released).
    pending_focus: Option<PaneId>,
    /// Saved layout for pane zoom toggle.
    saved_layout: Option<PaneLayout>,
}

impl PaneManager {
    /// Create a new pane manager with an initial pane.
    pub fn new(
        display: &mut crate::display::Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let pane_id = PaneId::new(0);

        let (terminal, notifier, master_fd, shell_pid) =
            Self::create_pane_terminal(display, &config, options, proxy, pane_id)?;

        let pane = Pane {
            terminal,
            notifier,
            search_state: SearchState::default(),
            inline_search_state: InlineSearchState::default(),
            suggestion_state: SuggestionState::default(),
            command_history: Default::default(),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        };

        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);

        let border_width = config.pane.border_width as f32;

        let min_pane_px = (
            config.pane.min_width as f32 * display.size_info.cell_width(),
            config.pane.min_height as f32 * display.size_info.cell_height(),
        );

        let window_bounds = PaneBounds {
            x: 0.,
            y: 0.,
            width: display.size_info.width(),
            height: display.size_info.height(),
        };

        let layout = PaneLayout::new(pane_id);

        let mut manager = Self {
            layout,
            panes,
            active_pane: pane_id,
            next_id: 1,
            window_bounds,
            pane_bounds: HashMap::new(),
            border_width,
            min_pane_px,
            config,
            pending_focus: None,
            saved_layout: None,
        };
        manager.recompute_bounds();
        Ok(manager)
    }

    /// Create a new pane (Term + PTY + event loop thread).
    fn create_pane_terminal(
        display: &mut crate::display::Display,
        config: &UiConfig,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
        pane_id: PaneId,
    ) -> Result<PaneTerminal, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let event_proxy = EventProxy::new(proxy, display.window.id());
        let window_id: u64 = display.window.id().into();

        let size_info = display.size_info;

        info!(
            "Creating pane {:?} with dimensions: {:?} x {:?}",
            pane_id,
            size_info.screen_lines(),
            size_info.columns()
        );

        let terminal = Term::new(config.term_options(), &size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = tty::new(&pty_config, size_info.into(), window_id)?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;

        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        if config.cursor.style().blinking {
            event_proxy.send_event(alacritty_terminal::event::Event::CursorBlinkingChange.into());
        }

        #[cfg(windows)]
        let master_fd = 0;
        #[cfg(windows)]
        let shell_pid = 0;

        Ok((terminal, Notifier(loop_tx), master_fd, shell_pid))
    }

    /// Get the active (focused) pane ID.
    pub fn active_pane_id(&self) -> PaneId {
        self.active_pane
    }

    /// Get a reference to the active pane.
    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.active_pane)
    }

    /// Get a mutable reference to a specific pane.
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// Get a reference to a specific pane.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Get all panes.
    pub fn panes(&self) -> &HashMap<PaneId, Pane> {
        &self.panes
    }

    /// Get pane bounds for a specific pane.
    pub fn pane_bounds(&self, id: PaneId) -> Option<PaneBounds> {
        self.pane_bounds.get(&id).copied()
    }

    /// Split the active pane in the given direction.
    pub fn split_active(
        &mut self,
        display: &mut crate::display::Display,
        proxy: EventLoopProxy<Event>,
        options: WindowOptions,
        direction: SplitDirection,
    ) -> Result<PaneId, Box<dyn Error>> {
        let new_id = PaneId::new(self.next_id);
        self.next_id += 1;

        let (terminal, notifier, master_fd, shell_pid) =
            Self::create_pane_terminal(display, &self.config, options, proxy, new_id)?;

        let pane = Pane {
            terminal,
            notifier,
            search_state: SearchState::default(),
            inline_search_state: InlineSearchState::default(),
            suggestion_state: SuggestionState::default(),
            command_history: Default::default(),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        };

        self.panes.insert(new_id, pane);

        if !self.layout.split(self.active_pane, new_id, direction) {
            // Should never happen, but handle gracefully.
            self.panes.remove(&new_id);
            return Err("Could not find active pane in layout".into());
        }

        self.recompute_bounds();
        Ok(new_id)
    }

    /// Close a pane by ID.
    pub fn close_pane(&mut self, target: PaneId) -> bool {
        if self.layout.count() <= 1 {
            return false; // Can't close the last pane
        }

        // Send shutdown to the event loop thread (may already be gone if PTY exited).
        if let Some(pane) = self.panes.get(&target) {
            let _ = pane.notifier.0.send(Msg::Shutdown);
        }

        if !self.layout.close(target) {
            return false;
        }

        self.panes.remove(&target);

        // Focus the first available pane.
        let ids = self.layout.pane_ids();
        if let Some(&first) = ids.first() {
            self.active_pane = first;
        }

        self.recompute_bounds();
        true
    }

    /// Queue a focus switch during event processing (applied after lock is released).
    pub fn request_focus(&mut self, target: PaneId) -> bool {
        if !self.panes.contains_key(&target) {
            return false;
        }
        self.pending_focus = Some(target);
        true
    }

    /// Get the queued focus target, if any.
    pub fn pending_focus(&self) -> Option<PaneId> {
        self.pending_focus
    }

    /// Apply a queued focus switch.
    pub fn apply_pending_focus(&mut self) -> bool {
        let Some(target) = self.pending_focus.take() else { return false };
        if !self.panes.contains_key(&target) {
            return false;
        }
        self.active_pane = target;
        true
    }

    /// Navigate focus in a direction.
    pub fn navigate_focus(&mut self, direction: SplitDirection) -> bool {
        if let Some(next) = self.layout.navigate(self.active_pane, direction) {
            self.request_focus(next)
        } else {
            false
        }
    }

    /// Resize a pane border.
    pub fn resize_pane(&mut self, target: PaneId, direction: SplitDirection, amount: f32) -> bool {
        let window_span = match direction {
            SplitDirection::Vertical => self.window_bounds.width - self.border_width,
            SplitDirection::Horizontal => self.window_bounds.height - self.border_width,
        };
        let dim = match direction {
            SplitDirection::Vertical => self.min_pane_px.0,
            SplitDirection::Horizontal => self.min_pane_px.1,
        };
        let min_ratio = (dim / window_span).clamp(0.05, 0.5);
        let max_ratio = 1.0 - min_ratio;
        let result = self.layout.resize(target, direction, amount, min_ratio, max_ratio);
        if result {
            self.recompute_bounds();
        }
        result
    }

    /// Resize by dragging a border to a pixel position.
    pub fn resize_drag(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
        px: f32,
        py: f32,
    ) -> bool {
        let pos = match direction {
            SplitDirection::Vertical => px,
            SplitDirection::Horizontal => py,
        };
        let dim = match direction {
            SplitDirection::Vertical => self.min_pane_px.0,
            SplitDirection::Horizontal => self.min_pane_px.1,
        };
        let result = self.layout.resize_drag(
            target,
            direction,
            pos,
            &self.window_bounds,
            self.border_width,
            dim,
        );
        if result {
            self.recompute_bounds();
        }
        result
    }

    /// Toggle pane zoom (maximize/restore the active pane).
    pub fn toggle_zoom(&mut self) {
        if let Some(saved) = self.saved_layout.take() {
            // Restore the saved layout.
            self.layout = saved;
        } else {
            // Save current layout and replace with just the active pane.
            self.saved_layout = Some(std::mem::take(&mut self.layout));
            self.layout = PaneLayout::Leaf(self.active_pane);
        }
        self.recompute_bounds();
    }

    /// Whether the active pane is currently zoomed.
    #[allow(dead_code)]
    pub fn is_zoomed(&self) -> bool {
        self.saved_layout.is_some()
    }

    /// Update window size and recompute all pane bounds.
    pub fn update_window_size(&mut self, display: &mut crate::display::Display) {
        self.window_bounds = PaneBounds {
            x: 0.,
            y: 0.,
            width: display.size_info.width(),
            height: display.size_info.height(),
        };
        self.recompute_bounds();
    }

    /// Get border width.
    pub fn border_width(&self) -> f32 {
        self.border_width
    }

    /// Find pane at pixel coordinates.
    pub fn pane_at(&self, px: f32, py: f32) -> Option<PaneId> {
        self.layout.pane_at(px, py, &self.window_bounds, self.border_width)
    }

    /// Find a draggable border at pixel coordinates.
    pub fn border_at(&self, px: f32, py: f32) -> Option<(PaneId, SplitDirection)> {
        self.layout.border_at(px, py, &self.window_bounds, self.border_width * 2.)
    }

    /// Find all draggable borders at a pixel position (for corner detection).
    pub fn corners_at(&self, px: f32, py: f32) -> Vec<(PaneId, SplitDirection)> {
        self.layout.corners_at(px, py, &self.window_bounds, self.border_width * 2.)
    }

    /// Number of panes.
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    /// Get pane IDs in layout order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.layout.pane_ids()
    }

    /// Recompute all pane bounds from the layout tree.
    fn recompute_bounds(&mut self) {
        self.pane_bounds.clear();
        let entries = self.layout.compute_bounds(self.window_bounds, self.border_width);
        for (id, bounds) in entries {
            self.pane_bounds.insert(id, bounds);
        }
    }

    /// Resize all pane terminals except the active one (whose lock is held externally).
    pub fn resize_terminals_except_active(&mut self, display: &crate::display::Display) {
        let active = self.active_pane;
        let cell_w = display.size_info.cell_width();
        let cell_h = display.size_info.cell_height();
        let padding_x = display.size_info.padding_x();
        let padding_y = display.size_info.padding_y();

        for (id, bounds) in &self.pane_bounds {
            if *id == active {
                continue;
            }
            let Some(pane) = self.panes.get_mut(id) else { continue };

            let inner = bounds.pad(padding_x, padding_y);
            let cols = ((inner.width / cell_w) as usize).max(1);
            let lines = ((inner.height / cell_h) as usize).max(1);

            let window_size = alacritty_terminal::event::WindowSize {
                num_cols: cols as u16,
                num_lines: lines as u16,
                cell_width: cell_w as u16,
                cell_height: cell_h as u16,
            };

            pane.notifier.on_resize(window_size);

            let mut terminal = pane.terminal.lock();
            terminal.resize(TermDimensions::new(cols, lines));
        }
    }

    /// Resize the active pane's terminal using the caller's existing lock.
    pub fn resize_active_terminal<T: alacritty_terminal::event::EventListener>(
        &mut self,
        display: &crate::display::Display,
        terminal: &mut alacritty_terminal::term::Term<T>,
    ) {
        let Some(bounds) = self.pane_bounds.get(&self.active_pane) else { return };

        let cell_w = display.size_info.cell_width();
        let cell_h = display.size_info.cell_height();
        let padding_x = display.size_info.padding_x();
        let padding_y = display.size_info.padding_y();
        let inner = bounds.pad(padding_x, padding_y);
        let cols = ((inner.width / cell_w) as usize).max(1);
        let lines = ((inner.height / cell_h) as usize).max(1);

        let window_size = alacritty_terminal::event::WindowSize {
            num_cols: cols as u16,
            num_lines: lines as u16,
            cell_width: cell_w as u16,
            cell_height: cell_h as u16,
        };

        if let Some(pane) = self.panes.get_mut(&self.active_pane) {
            pane.notifier.on_resize(window_size);
        }

        terminal.resize(TermDimensions::new(cols, lines));
    }
}
