//! Terminal window context.

use std::cell::RefCell;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::RawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use serde_json as json;
use winit::event::{Event as WinitEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use alacritty_terminal::event::Event as TerminalEvent;
use alacritty_terminal::event_loop::{Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::display::window::Window;
use crate::display::{Display, DrawContext};
use crate::event::{
    ActionContext, CommandHistory, Event, EventProxy, EventType, InlineSearchState, Mouse,
    SearchState, SuggestionState, TouchPurpose,
};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::pane::manager::PaneManager;
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::{input, renderer};

/// Event context for one individual Alacritty window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    pane_manager: Option<PaneManager>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    modifiers: Modifiers,
    inline_search_state: InlineSearchState,
    search_state: SearchState,
    suggestion_state: SuggestionState,
    command_history: RefCell<CommandHistory>,
    notifier: Notifier,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    preserve_title: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
}

impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, gl_context, &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;

        let display = Display::new(window, gl_context, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        mut display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let preserve_title = options.window_identity.title.is_some();

        let pane_manager = PaneManager::new(&mut display, config.clone(), options, proxy.clone())?;
        let active_pane = pane_manager.active_pane().expect("new pane manager has active pane");

        let terminal = Arc::clone(&active_pane.terminal);
        let notifier = active_pane.notifier.clone();
        #[cfg(not(windows))]
        let master_fd = active_pane.master_fd;
        #[cfg(not(windows))]
        let shell_pid = active_pane.shell_pid;

        // Create context for the Alacritty window.
        Ok(WindowContext {
            preserve_title,
            terminal,
            display,
            pane_manager: Some(pane_manager),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            config,
            notifier,
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            inline_search_state: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
            suggestion_state: Default::default(),
            command_history: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        if let Some(ref pm) = self.pane_manager {
            for pane_id in pm.pane_ids() {
                if let Some(pane) = pm.pane(pane_id) {
                    pane.terminal.lock().set_options(self.config.term_options());
                }
            }
        } else {
            self.terminal.lock().set_options(self.config.term_options());
        }

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Close the active pane in this window, falling back to the old pane if it was the last.
    /// Returns true if a pane was closed, false if there was only one pane.
    pub fn close_active_pane(&mut self) -> bool {
        let pm = match self.pane_manager.as_mut() {
            Some(pm) if pm.len() > 1 => pm,
            _ => return false,
        };
        pm.close_pane(pm.active_pane_id());
        // Sync window context terminal to the new active pane.
        if let Some(active) = pm.active_pane() {
            self.terminal = Arc::clone(&active.terminal);
            self.notifier = active.notifier.clone();
            #[cfg(not(windows))]
            {
                self.master_fd = active.master_fd;
                self.shell_pid = active.shell_pid;
            }
        }
        true
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses alacritty's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window — get terminal from active pane to avoid stale ref.
        let terminal_arc = match self.pane_manager {
            Some(ref pm) => Arc::clone(&pm.active_pane().expect("active pane must exist").terminal),
            None => Arc::clone(&self.terminal),
        };
        let terminal = terminal_arc.lock();
        self.display.draw(
            terminal,
            DrawContext {
                pane_manager: self.pane_manager.as_ref(),
                scheduler,
                message_buffer: &self.message_buffer,
                config: &self.config,
                search_state: &mut self.search_state,
                suggestion_state: &self.suggestion_state,
            },
        );
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let terminal_arc = match self.pane_manager {
            Some(ref pm) => Arc::clone(&pm.active_pane().expect("active pane must exist").terminal),
            None => Arc::clone(&self.terminal),
        };
        let mut terminal = terminal_arc.lock();

        let old_is_searching = self.search_state.history_index.is_some();

        let context = ActionContext {
            cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
            prev_bell_cmd: &mut self.prev_bell_cmd,
            message_buffer: &mut self.message_buffer,
            inline_search_state: &mut self.inline_search_state,
            search_state: &mut self.search_state,
            suggestion_state: &mut self.suggestion_state,
            command_history: &self.command_history,
            modifiers: &mut self.modifiers,
            notifier: &mut self.notifier,
            pane_manager: self.pane_manager.as_mut(),
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: self.master_fd,
            #[cfg(not(windows))]
            shell_pid: self.shell_pid,
            preserve_title: self.preserve_title,
            config: &self.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        {
            let mut processor = input::Processor::new(context);

            for event in self.event_queue.drain(..) {
                processor.handle_event(event);
            }
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            Self::submit_display_update(
                &mut terminal,
                &mut self.display,
                &mut self.notifier,
                &self.message_buffer,
                &mut self.search_state,
                old_is_searching,
                &self.config,
            );
            self.dirty = true;

            // Update pane geometry on window resize.
            if let Some(ref mut pm) = self.pane_manager {
                pm.update_window_size(&mut self.display);
                pm.resize_terminals_except_active(&self.display);
                pm.resize_active_terminal(&self.display, &mut terminal);
            }
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &self.config,
                &self.mouse,
                self.modifiers.state(),
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Drop terminal lock so we can safely re-lock after applying pending focus.
        drop(terminal);

        // Apply pending focus before drawing so the correct terminal is rendered.
        let old_active_pane = self.pane_manager.as_ref().map(|pm| pm.active_pane_id());

        // Save per-pane state for the pane we're switching away from.
        if let Some(old_id) = old_active_pane {
            let saved_search = self.search_state.clone();
            let saved_inline_search = self.inline_search_state.clone();
            let saved_suggestion = self.suggestion_state.clone();
            let saved_history = self.command_history.borrow().clone();
            if let Some(ref mut pm) = self.pane_manager {
                if let Some(pane) = pm.pane_mut(old_id) {
                    pane.search_state = saved_search;
                    pane.inline_search_state = saved_inline_search;
                    pane.suggestion_state = saved_suggestion;
                    *pane.command_history.borrow_mut() = saved_history;
                }
            }
        }

        if let Some(ref mut pm) = self.pane_manager {
            pm.apply_pending_focus();
        }

        // Sync stale terminal/notifier to the current active pane.
        let new_active_pane = self.pane_manager.as_ref().map(|pm| pm.active_pane_id());
        if let Some(ref pm) = self.pane_manager {
            if let Some(active) = pm.active_pane() {
                self.terminal = Arc::clone(&active.terminal);
                self.notifier = active.notifier.clone();
                #[cfg(not(windows))]
                {
                    self.master_fd = active.master_fd;
                    self.shell_pid = active.shell_pid;
                }

                // Restore per-pane state for the newly active pane.
                self.search_state = active.search_state.clone();
                self.inline_search_state = active.inline_search_state.clone();
                self.suggestion_state = active.suggestion_state.clone();
                *self.command_history.borrow_mut() = active.command_history.borrow().clone();

                // Update mouse pane origin so coordinate transforms use the new pane's offset.
                if let Some(bounds) = pm.pane_bounds(pm.active_pane_id()) {
                    self.mouse.pane_origin_x = bounds.x;
                    self.mouse.pane_origin_y = bounds.y;
                }
            }
        }

        // Re-lock the correct terminal for drawing (after focus is settled).
        let terminal_arc = match self.pane_manager {
            Some(ref pm) => Arc::clone(&pm.active_pane().expect("active pane must exist").terminal),
            None => Arc::clone(&self.terminal),
        };
        let terminal = terminal_arc.lock();

        // Draw with the correct terminal.
        self.display.draw(
            terminal,
            DrawContext {
                pane_manager: self.pane_manager.as_ref(),
                scheduler,
                message_buffer: &self.message_buffer,
                config: &self.config,
                search_state: &mut self.search_state,
                suggestion_state: &self.suggestion_state,
            },
        );

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }

        // Reset cursor blink state when focus switches to a different pane.
        if let (Some(old), Some(new)) = (old_active_pane, new_active_pane) {
            if old != new {
                if let Some(ref pm) = self.pane_manager {
                    if let Some(active) = pm.active_pane() {
                        let mut new_terminal = active.terminal.lock();

                        // Propagate window focus state to the newly active pane, since
                        // WindowEvent::Focused only updates the terminal that was active
                        // at the time the event was processed.
                        new_terminal.is_focused = true;

                        let window_id = self.display.window.id();
                        scheduler.unschedule(TimerId::new(Topic::BlinkCursor, window_id));
                        scheduler.unschedule(TimerId::new(Topic::BlinkTimeout, window_id));
                        self.cursor_blink_timed_out = false;

                        // Determine if cursor should blink for this pane.
                        let mut cursor_style = self.config.cursor.style;
                        let vi_mode = new_terminal.mode().contains(TermMode::VI);
                        if vi_mode {
                            cursor_style = self.config.cursor.vi_mode_style.unwrap_or(cursor_style);
                        }
                        let terminal_blinking = new_terminal.cursor_style().blinking;
                        let mut blinking =
                            cursor_style.blinking_override().unwrap_or(terminal_blinking);
                        blinking &= (vi_mode
                            || new_terminal.mode().contains(TermMode::SHOW_CURSOR))
                            && self.display.ime.preedit().is_none();

                        if blinking {
                            self.display.cursor_hidden = false;
                            self.dirty = true;

                            let interval =
                                Duration::from_millis(self.config.cursor.blink_interval());
                            let timer_id = TimerId::new(Topic::BlinkCursor, window_id);
                            let event = Event::new(EventType::BlinkCursor, window_id);
                            scheduler.schedule(event, interval, true, timer_id);

                            let timeout = self.config.cursor.blink_timeout();
                            if timeout != Duration::ZERO {
                                let timeout_id = TimerId::new(Topic::BlinkTimeout, window_id);
                                let timeout_event =
                                    Event::new(EventType::BlinkCursorTimeout, window_id);
                                scheduler.schedule(timeout_event, timeout, false, timeout_id);
                            }
                        } else {
                            self.display.cursor_hidden = false;
                            self.dirty = true;
                        }
                    }
                }
            }
        }
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        terminal: &mut Term<EventProxy>,
        display: &mut Display,
        notifier: &mut Notifier,
        message_buffer: &MessageBuffer,
        search_state: &mut SearchState,
        old_is_searching: bool,
        config: &UiConfig,
    ) {
        // Compute cursor positions before resize.
        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            search_state.direction == Direction::Left
        };

        display.handle_update(terminal, notifier, message_buffer, search_state, config);

        let new_is_searching = search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            // Scroll on search start to make sure origin is visible with minimal viewport motion.
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Shutdown all panes' PTYs.
        if let Some(ref pm) = self.pane_manager {
            for pane_id in pm.pane_ids() {
                if let Some(pane) = pm.pane(pane_id) {
                    let _ = pane.notifier.0.send(Msg::Shutdown);
                }
            }
        } else {
            let _ = self.notifier.0.send(Msg::Shutdown);
        }
    }
}
