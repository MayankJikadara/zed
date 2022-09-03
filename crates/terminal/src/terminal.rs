pub mod mappings;
pub mod modal;
pub mod terminal_container_view;
pub mod terminal_element;
pub mod terminal_view;
// procinfo = { git = "https://github.com/zed-industries/wezterm", rev = "40a7dbf93542fbe4178c2e4b4bd438126a6432b9", default-features = false }
use alacritty_terminal::{
    ansi::{ClearMode, Handler},
    config::{Config, Program, PtyConfig, Scrolling},
    event::{Event as AlacTermEvent, EventListener, Notify, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll as AlacScroll},
    index::{Column, Direction as AlacDirection, Line, Point},
    selection::{Selection, SelectionRange, SelectionType},
    sync::FairMutex,
    term::{
        cell::Cell,
        color::Rgb,
        search::{Match, RegexIter, RegexSearch},
        RenderableCursor, TermMode,
    },
    tty::{self, setup_env},
    Term,
};
use anyhow::{bail, Result};

use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    FutureExt,
};

use mappings::mouse::{
    alt_scroll, mouse_button_report, mouse_moved_report, mouse_point, mouse_side, scroll_report,
};
use modal::deploy_modal;

use procinfo::LocalProcessInfo;
use settings::{AlternateScroll, Settings, Shell, TerminalBlink};

use std::{
    collections::{HashMap, VecDeque},
    fmt::Display,
    ops::{Deref, RangeInclusive, Sub},
    os::unix::prelude::AsRawFd,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;

use gpui::{
    geometry::vector::{vec2f, Vector2F},
    keymap::Keystroke,
    scene::{ClickRegionEvent, DownRegionEvent, DragRegionEvent, UpRegionEvent},
    ClipboardItem, Entity, ModelContext, MouseButton, MouseMovedEvent, MutableAppContext,
    ScrollWheelEvent, Task,
};

use crate::mappings::{
    colors::{get_color_at_index, to_alac_rgb},
    keys::to_esc_str,
};

///Initialize and register all of our action handlers
pub fn init(cx: &mut MutableAppContext) {
    cx.add_action(deploy_modal);

    terminal_view::init(cx);
    terminal_container_view::init(cx);
}

///Scrolling is unbearably sluggish by default. Alacritty supports a configurable
///Scroll multiplier that is set to 3 by default. This will be removed when I
///Implement scroll bars.
const SCROLL_MULTIPLIER: f32 = 4.;
const MAX_SEARCH_LINES: usize = 100;
const DEBUG_TERMINAL_WIDTH: f32 = 500.;
const DEBUG_TERMINAL_HEIGHT: f32 = 30.;
const DEBUG_CELL_WIDTH: f32 = 5.;
const DEBUG_LINE_HEIGHT: f32 = 5.;

///Upward flowing events, for changing the title and such
#[derive(Clone, Copy, Debug)]
pub enum Event {
    TitleChanged,
    CloseTerminal,
    Bell,
    Wakeup,
    BlinkChanged,
    SelectionsChanged,
}

#[derive(Clone)]
enum InternalEvent {
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),
    Resize(TerminalSize),
    Clear,
    // FocusNextMatch,
    Scroll(AlacScroll),
    ScrollToPoint(Point),
    SetSelection(Option<(Selection, Point)>),
    UpdateSelection(Vector2F),
    Copy,
}

///A translation struct for Alacritty to communicate with us from their event loop
#[derive(Clone)]
pub struct ZedListener(UnboundedSender<AlacTermEvent>);

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(event).ok();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    cell_width: f32,
    line_height: f32,
    height: f32,
    width: f32,
}

impl TerminalSize {
    pub fn new(line_height: f32, cell_width: f32, size: Vector2F) -> Self {
        TerminalSize {
            cell_width,
            line_height,
            width: size.x(),
            height: size.y(),
        }
    }

    pub fn num_lines(&self) -> usize {
        (self.height / self.line_height).floor() as usize
    }

    pub fn num_columns(&self) -> usize {
        (self.width / self.cell_width).floor() as usize
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn cell_width(&self) -> f32 {
        self.cell_width
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }
}
impl Default for TerminalSize {
    fn default() -> Self {
        TerminalSize::new(
            DEBUG_LINE_HEIGHT,
            DEBUG_CELL_WIDTH,
            vec2f(DEBUG_TERMINAL_WIDTH, DEBUG_TERMINAL_HEIGHT),
        )
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(val: TerminalSize) -> Self {
        WindowSize {
            num_lines: val.num_lines() as u16,
            num_cols: val.num_columns() as u16,
            cell_width: val.cell_width() as u16,
            cell_height: val.line_height() as u16,
        }
    }
}

impl Dimensions for TerminalSize {
    /// Note: this is supposed to be for the back buffer's length,
    /// but we exclusively use it to resize the terminal, which does not
    /// use this method. We still have to implement it for the trait though,
    /// hence, this comment.
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

#[derive(Error, Debug)]
pub struct TerminalError {
    pub directory: Option<PathBuf>,
    pub shell: Option<Shell>,
    pub source: std::io::Error,
}

impl TerminalError {
    pub fn fmt_directory(&self) -> String {
        self.directory
            .clone()
            .map(|path| {
                match path
                    .into_os_string()
                    .into_string()
                    .map_err(|os_str| format!("<non-utf8 path> {}", os_str.to_string_lossy()))
                {
                    Ok(s) => s,
                    Err(s) => s,
                }
            })
            .unwrap_or_else(|| {
                let default_dir =
                    dirs::home_dir().map(|buf| buf.into_os_string().to_string_lossy().to_string());
                match default_dir {
                    Some(dir) => format!("<none specified, using home directory> {}", dir),
                    None => "<none specified, could not find home directory>".to_string(),
                }
            })
    }

    pub fn shell_to_string(&self) -> Option<String> {
        self.shell.as_ref().map(|shell| match shell {
            Shell::System => "<system shell>".to_string(),
            Shell::Program(p) => p.to_string(),
            Shell::WithArguments { program, args } => format!("{} {}", program, args.join(" ")),
        })
    }

    pub fn fmt_shell(&self) -> String {
        self.shell
            .clone()
            .map(|shell| match shell {
                Shell::System => "<system defined shell>".to_string(),

                Shell::Program(s) => s,
                Shell::WithArguments { program, args } => format!("{} {}", program, args.join(" ")),
            })
            .unwrap_or_else(|| "<none specified, using system defined shell>".to_string())
    }
}

impl Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir_string: String = self.fmt_directory();
        let shell = self.fmt_shell();

        write!(
            f,
            "Working directory: {} Shell command: `{}`, IOError: {}",
            dir_string, shell, self.source
        )
    }
}

pub struct TerminalBuilder {
    terminal: Terminal,
    events_rx: UnboundedReceiver<AlacTermEvent>,
}

impl TerminalBuilder {
    pub fn new(
        working_directory: Option<PathBuf>,
        shell: Option<Shell>,
        env: Option<HashMap<String, String>>,
        initial_size: TerminalSize,
        blink_settings: Option<TerminalBlink>,
        alternate_scroll: &AlternateScroll,
    ) -> Result<TerminalBuilder> {
        let pty_config = {
            let alac_shell = shell.clone().and_then(|shell| match shell {
                Shell::System => None,
                Shell::Program(program) => Some(Program::Just(program)),
                Shell::WithArguments { program, args } => Some(Program::WithArgs { program, args }),
            });

            PtyConfig {
                shell: alac_shell,
                working_directory: working_directory.clone(),
                hold: false,
            }
        };

        let mut env = env.unwrap_or_default();

        //TODO: Properly set the current locale,
        env.insert("LC_ALL".to_string(), "en_US.UTF-8".to_string());

        let alac_scrolling = Scrolling::default();
        // alac_scrolling.set_history((BACK_BUFFER_SIZE * 2) as u32);

        let config = Config {
            pty_config: pty_config.clone(),
            env,
            scrolling: alac_scrolling,
            ..Default::default()
        };

        setup_env(&config);

        //Spawn a task so the Alacritty EventLoop can communicate with us in a view context
        //TODO: Remove with a bounded sender which can be dispatched on &self
        let (events_tx, events_rx) = unbounded();
        //Set up the terminal...
        let mut term = Term::new(&config, &initial_size, ZedListener(events_tx.clone()));

        //Start off blinking if we need to
        if let Some(TerminalBlink::On) = blink_settings {
            term.set_mode(alacritty_terminal::ansi::Mode::BlinkingCursor)
        }

        //Alacritty defaults to alternate scrolling being on, so we just need to turn it off.
        if let AlternateScroll::Off = alternate_scroll {
            term.unset_mode(alacritty_terminal::ansi::Mode::AlternateScroll)
        }

        let term = Arc::new(FairMutex::new(term));

        //Setup the pty...
        let pty = match tty::new(&pty_config, initial_size.into(), None) {
            Ok(pty) => pty,
            Err(error) => {
                bail!(TerminalError {
                    directory: working_directory,
                    shell,
                    source: error,
                });
            }
        };

        let fd = pty.file().as_raw_fd();
        let shell_pid = pty.child().id();

        //And connect them together
        let event_loop = EventLoop::new(
            term.clone(),
            ZedListener(events_tx.clone()),
            pty,
            pty_config.hold,
            false,
        );

        //Kick things off
        let pty_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        let terminal = Terminal {
            pty_tx: Notifier(pty_tx),
            term,
            events: VecDeque::with_capacity(10), //Should never get this high.
            last_content: Default::default(),
            cur_size: initial_size,
            last_mouse: None,
            matches: Vec::new(),
            last_synced: Instant::now(),
            sync_task: None,
            selection_head: None,
            shell_fd: fd as u32,
            shell_pid,
            foreground_process_info: None,
            breadcrumb_text: String::new(),
            scroll_px: 0.,
        };

        Ok(TerminalBuilder {
            terminal,
            events_rx,
        })
    }

    pub fn subscribe(mut self, cx: &mut ModelContext<Terminal>) -> Terminal {
        //Event loop
        cx.spawn_weak(|this, mut cx| async move {
            use futures::StreamExt;

            while let Some(event) = self.events_rx.next().await {
                this.upgrade(&cx)?.update(&mut cx, |this, cx| {
                    //Process the first event immediately for lowered latency
                    this.process_event(&event, cx);
                });

                'outer: loop {
                    let mut events = vec![];
                    let mut timer = cx.background().timer(Duration::from_millis(4)).fuse();

                    loop {
                        futures::select_biased! {
                            _ = timer => break,
                            event = self.events_rx.next() => {
                                if let Some(event) = event {
                                    events.push(event);
                                    if events.len() > 100 {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            },
                        }
                    }

                    if events.is_empty() {
                        smol::future::yield_now().await;
                        break 'outer;
                    } else {
                        this.upgrade(&cx)?.update(&mut cx, |this, cx| {
                            for event in events {
                                this.process_event(&event, cx);
                            }
                        });
                        smol::future::yield_now().await;
                    }
                }
            }

            Some(())
        })
        .detach();

        self.terminal
    }
}

#[derive(Debug, Clone)]
struct IndexedCell {
    point: Point,
    cell: Cell,
}

impl Deref for IndexedCell {
    type Target = Cell;

    #[inline]
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

#[derive(Clone)]
pub struct TerminalContent {
    cells: Vec<IndexedCell>,
    mode: TermMode,
    display_offset: usize,
    selection_text: Option<String>,
    selection: Option<SelectionRange>,
    cursor: RenderableCursor,
    cursor_char: char,
}

impl Default for TerminalContent {
    fn default() -> Self {
        TerminalContent {
            cells: Default::default(),
            mode: Default::default(),
            display_offset: Default::default(),
            selection_text: Default::default(),
            selection: Default::default(),
            cursor: RenderableCursor {
                shape: alacritty_terminal::ansi::CursorShape::Block,
                point: Point::new(Line(0), Column(0)),
            },
            cursor_char: Default::default(),
        }
    }
}

pub struct Terminal {
    pty_tx: Notifier,
    term: Arc<FairMutex<Term<ZedListener>>>,
    events: VecDeque<InternalEvent>,
    last_mouse: Option<(Point, AlacDirection)>,
    pub matches: Vec<RangeInclusive<Point>>,
    cur_size: TerminalSize,
    last_content: TerminalContent,
    last_synced: Instant,
    sync_task: Option<Task<()>>,
    selection_head: Option<Point>,
    breadcrumb_text: String,
    shell_pid: u32,
    shell_fd: u32,
    foreground_process_info: Option<LocalProcessInfo>,
    scroll_px: f32,
}

impl Terminal {
    fn process_event(&mut self, event: &AlacTermEvent, cx: &mut ModelContext<Self>) {
        match event {
            AlacTermEvent::Title(title) => {
                self.breadcrumb_text = title.to_string();
            }
            AlacTermEvent::ResetTitle => {
                self.breadcrumb_text = String::new();
            }
            AlacTermEvent::ClipboardStore(_, data) => {
                cx.write_to_clipboard(ClipboardItem::new(data.to_string()))
            }
            AlacTermEvent::ClipboardLoad(_, format) => self.write_to_pty(format(
                &cx.read_from_clipboard()
                    .map(|ci| ci.text().to_string())
                    .unwrap_or_else(|| "".to_string()),
            )),
            AlacTermEvent::PtyWrite(out) => self.write_to_pty(out.clone()),
            AlacTermEvent::TextAreaSizeRequest(format) => {
                self.write_to_pty(format(self.cur_size.into()))
            }
            AlacTermEvent::CursorBlinkingChange => {
                cx.emit(Event::BlinkChanged);
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit => cx.emit(Event::CloseTerminal),
            AlacTermEvent::MouseCursorDirty => {
                //NOOP, Handled in render
            }
            AlacTermEvent::Wakeup => {
                cx.emit(Event::Wakeup);

                dbg!("*********");
                if self.update_process_info() {
                    cx.emit(Event::TitleChanged)
                }
            }
            AlacTermEvent::ColorRequest(idx, fun_ptr) => {
                self.events
                    .push_back(InternalEvent::ColorRequest(*idx, fun_ptr.clone()));
            }
        }
    }

    /// Update the cached process info, returns whether the Zed-relevant info has changed
    fn update_process_info(&mut self) -> bool {
        let mut pid = unsafe { libc::tcgetpgrp(self.shell_fd as i32) };
        if pid < 0 {
            pid = self.shell_pid as i32;
        }

        if let Some(process_info) = LocalProcessInfo::with_root_pid(pid as u32) {
            let res = self
                .foreground_process_info
                .as_ref()
                .map(|old_info| {
                    process_info.cwd != old_info.cwd || process_info.name != old_info.name
                })
                .unwrap_or(true);

            self.foreground_process_info = Some(process_info.clone());

            res
        } else {
            false
        }
    }

    ///Takes events from Alacritty and translates them to behavior on this view
    fn process_terminal_event(
        &mut self,
        event: &InternalEvent,
        term: &mut Term<ZedListener>,
        cx: &mut ModelContext<Self>,
    ) {
        match event {
            InternalEvent::ColorRequest(index, format) => {
                let color = term.colors()[*index].unwrap_or_else(|| {
                    let term_style = &cx.global::<Settings>().theme.terminal;
                    to_alac_rgb(get_color_at_index(index, &term_style.colors))
                });
                self.write_to_pty(format(color))
            }
            InternalEvent::Resize(new_size) => {
                self.cur_size = *new_size;

                self.pty_tx.0.send(Msg::Resize((*new_size).into())).ok();

                term.resize(*new_size);
            }
            InternalEvent::Clear => {
                self.write_to_pty("\x0c".to_string());
                term.clear_screen(ClearMode::Saved);
            }
            InternalEvent::Scroll(scroll) => {
                term.scroll_display(*scroll);
            }
            InternalEvent::SetSelection(selection) => {
                term.selection = selection.as_ref().map(|(sel, _)| sel.clone());

                if let Some((_, head)) = selection {
                    self.selection_head = Some(*head);
                }
                cx.emit(Event::SelectionsChanged)
            }
            InternalEvent::UpdateSelection(position) => {
                if let Some(mut selection) = term.selection.take() {
                    let point = mouse_point(*position, self.cur_size, term.grid().display_offset());
                    let side = mouse_side(*position, self.cur_size);

                    selection.update(point, side);
                    term.selection = Some(selection);

                    self.selection_head = Some(point);
                    cx.emit(Event::SelectionsChanged)
                }
            }

            InternalEvent::Copy => {
                if let Some(txt) = term.selection_to_string() {
                    cx.write_to_clipboard(ClipboardItem::new(txt))
                }
            }
            InternalEvent::ScrollToPoint(point) => term.scroll_to_point(*point),
        }
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    //To test:
    //- Activate match on terminal (scrolling and selection)
    //- Editor search snapping behavior

    pub fn activate_match(&mut self, index: usize) {
        if let Some(search_match) = self.matches.get(index).cloned() {
            self.set_selection(Some((make_selection(&search_match), *search_match.end())));

            self.events
                .push_back(InternalEvent::ScrollToPoint(*search_match.start()));
        }
    }

    fn set_selection(&mut self, selection: Option<(Selection, Point)>) {
        self.events
            .push_back(InternalEvent::SetSelection(selection));
    }

    pub fn copy(&mut self) {
        self.events.push_back(InternalEvent::Copy);
    }

    pub fn clear(&mut self) {
        self.events.push_back(InternalEvent::Clear)
    }

    ///Resize the terminal and the PTY.
    pub fn set_size(&mut self, new_size: TerminalSize) {
        self.events.push_back(InternalEvent::Resize(new_size))
    }

    ///Write the Input payload to the tty.
    fn write_to_pty(&self, input: String) {
        self.pty_tx.notify(input.into_bytes());
    }

    pub fn input(&mut self, input: String) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Bottom));
        self.events.push_back(InternalEvent::SetSelection(None));

        self.write_to_pty(input);
    }

    pub fn try_keystroke(&mut self, keystroke: &Keystroke) -> bool {
        let esc = to_esc_str(keystroke, &self.last_content.mode);
        if let Some(esc) = esc {
            self.input(esc);
            true
        } else {
            false
        }
    }

    ///Paste text into the terminal
    pub fn paste(&mut self, text: &str) {
        let paste_text = if self.last_content.mode.contains(TermMode::BRACKETED_PASTE) {
            format!("{}{}{}", "\x1b[200~", text.replace('\x1b', ""), "\x1b[201~")
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.input(paste_text)
    }

    pub fn try_sync(&mut self, cx: &mut ModelContext<Self>) {
        let term = self.term.clone();

        let mut terminal = if let Some(term) = term.try_lock_unfair() {
            term
        } else if self.last_synced.elapsed().as_secs_f32() > 0.25 {
            term.lock_unfair() //It's been too long, force block
        } else if let None = self.sync_task {
            //Skip this frame
            let delay = cx.background().timer(Duration::from_millis(16));
            self.sync_task = Some(cx.spawn_weak(|weak_handle, mut cx| async move {
                delay.await;
                cx.update(|cx| {
                    if let Some(handle) = weak_handle.upgrade(cx) {
                        handle.update(cx, |terminal, cx| {
                            terminal.sync_task.take();
                            cx.notify();
                        });
                    }
                });
            }));
            return;
        } else {
            //No lock and delayed rendering already scheduled, nothing to do
            return;
        };

        if self.update_process_info() {
            cx.emit(Event::TitleChanged);
        }

        //Note that the ordering of events matters for event processing
        while let Some(e) = self.events.pop_front() {
            self.process_terminal_event(&e, &mut terminal, cx)
        }

        self.last_content = Self::make_content(&terminal);
        self.last_synced = Instant::now();
    }

    fn make_content(term: &Term<ZedListener>) -> TerminalContent {
        let content = term.renderable_content();
        TerminalContent {
            cells: content
                .display_iter
                //TODO: Add this once there's a way to retain empty lines
                // .filter(|ic| {
                //     !ic.flags.contains(Flags::HIDDEN)
                //         && !(ic.bg == Named(NamedColor::Background)
                //             && ic.c == ' '
                //             && !ic.flags.contains(Flags::INVERSE))
                // })
                .map(|ic| IndexedCell {
                    point: ic.point,
                    cell: ic.cell.clone(),
                })
                .collect::<Vec<IndexedCell>>(),
            mode: content.mode,
            display_offset: content.display_offset,
            selection_text: term.selection_to_string(),
            selection: content.selection,
            cursor: content.cursor,
            cursor_char: term.grid()[content.cursor.point].c,
        }
    }

    pub fn focus_in(&self) {
        if self.last_content.mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[I".to_string());
        }
    }

    pub fn focus_out(&self) {
        if self.last_content.mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[O".to_string());
        }
    }

    pub fn mouse_changed(&mut self, point: Point, side: AlacDirection) -> bool {
        match self.last_mouse {
            Some((old_point, old_side)) => {
                if old_point == point && old_side == side {
                    false
                } else {
                    self.last_mouse = Some((point, side));
                    true
                }
            }
            None => {
                self.last_mouse = Some((point, side));
                true
            }
        }
    }

    pub fn mouse_mode(&self, shift: bool) -> bool {
        self.last_content.mode.intersects(TermMode::MOUSE_MODE) && !shift
    }

    pub fn mouse_move(&mut self, e: &MouseMovedEvent, origin: Vector2F) {
        let position = e.position.sub(origin);

        let point = mouse_point(position, self.cur_size, self.last_content.display_offset);
        let side = mouse_side(position, self.cur_size);

        if self.mouse_changed(point, side) && self.mouse_mode(e.shift) {
            if let Some(bytes) = mouse_moved_report(point, e, self.last_content.mode) {
                self.pty_tx.notify(bytes);
            }
        }
    }

    pub fn mouse_drag(&mut self, e: DragRegionEvent, origin: Vector2F) {
        let position = e.position.sub(origin);

        if !self.mouse_mode(e.shift) {
            // Alacritty has the same ordering, of first updating the selection
            // then scrolling 15ms later
            self.events
                .push_back(InternalEvent::UpdateSelection(position));

            // Doesn't make sense to scroll the alt screen
            if !self.last_content.mode.contains(TermMode::ALT_SCREEN) {
                let scroll_delta = match self.drag_line_delta(e) {
                    Some(value) => value,
                    None => return,
                };

                let scroll_lines = (scroll_delta / self.cur_size.line_height) as i32;

                self.events
                    .push_back(InternalEvent::Scroll(AlacScroll::Delta(scroll_lines)));
                self.events
                    .push_back(InternalEvent::UpdateSelection(position))
            }
        }
    }

    fn drag_line_delta(&mut self, e: DragRegionEvent) -> Option<f32> {
        //TODO: Why do these need to be doubled? Probably the same problem that the IME has
        let top = e.region.origin_y() + (self.cur_size.line_height * 2.);
        let bottom = e.region.lower_left().y() - (self.cur_size.line_height * 2.);
        let scroll_delta = if e.position.y() < top {
            (top - e.position.y()).powf(1.1)
        } else if e.position.y() > bottom {
            -((e.position.y() - bottom).powf(1.1))
        } else {
            return None; //Nothing to do
        };
        Some(scroll_delta)
    }

    pub fn mouse_down(&mut self, e: &DownRegionEvent, origin: Vector2F) {
        let position = e.position.sub(origin);
        let point = mouse_point(position, self.cur_size, self.last_content.display_offset);
        let side = mouse_side(position, self.cur_size);

        if self.mouse_mode(e.shift) {
            if let Some(bytes) = mouse_button_report(point, e, true, self.last_content.mode) {
                self.pty_tx.notify(bytes);
            }
        } else if e.button == MouseButton::Left {
            self.events.push_back(InternalEvent::SetSelection(Some((
                Selection::new(SelectionType::Simple, point, side),
                point,
            ))));
        }
    }

    pub fn left_click(&mut self, e: &ClickRegionEvent, origin: Vector2F) {
        let position = e.position.sub(origin);

        if !self.mouse_mode(e.shift) {
            let point = mouse_point(position, self.cur_size, self.last_content.display_offset);
            let side = mouse_side(position, self.cur_size);

            let selection_type = match e.click_count {
                0 => return, //This is a release
                1 => Some(SelectionType::Simple),
                2 => Some(SelectionType::Semantic),
                3 => Some(SelectionType::Lines),
                _ => None,
            };

            let selection =
                selection_type.map(|selection_type| Selection::new(selection_type, point, side));

            if let Some(sel) = selection {
                self.events
                    .push_back(InternalEvent::SetSelection(Some((sel, point))));
            }
        }
    }

    pub fn mouse_up(&mut self, e: &UpRegionEvent, origin: Vector2F) {
        let position = e.position.sub(origin);
        if self.mouse_mode(e.shift) {
            let point = mouse_point(position, self.cur_size, self.last_content.display_offset);

            if let Some(bytes) = mouse_button_report(point, e, false, self.last_content.mode) {
                self.pty_tx.notify(bytes);
            }
        } else if e.button == MouseButton::Left {
            // Seems pretty standard to automatically copy on mouse_up for terminals,
            // so let's do that here
            self.copy();
        }
        self.last_mouse = None;
    }

    ///Scroll the terminal
    pub fn scroll_wheel(&mut self, e: &ScrollWheelEvent, origin: Vector2F) {
        let mouse_mode = self.mouse_mode(e.shift);

        if let Some(scroll_lines) = self.determine_scroll_lines(e, mouse_mode) {
            if mouse_mode {
                let point = mouse_point(
                    e.position.sub(origin),
                    self.cur_size,
                    self.last_content.display_offset,
                );

                if let Some(scrolls) =
                    scroll_report(point, scroll_lines as i32, e, self.last_content.mode)
                {
                    for scroll in scrolls {
                        self.pty_tx.notify(scroll);
                    }
                };
            } else if self
                .last_content
                .mode
                .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                && !e.shift
            {
                self.pty_tx.notify(alt_scroll(scroll_lines))
            } else {
                if scroll_lines != 0 {
                    let scroll = AlacScroll::Delta(scroll_lines);

                    self.events.push_back(InternalEvent::Scroll(scroll));
                }
            }
        }
    }

    fn determine_scroll_lines(&mut self, e: &ScrollWheelEvent, mouse_mode: bool) -> Option<i32> {
        let scroll_multiplier = if mouse_mode { 1. } else { SCROLL_MULTIPLIER };

        match e.phase {
            /* Reset scroll state on started */
            Some(gpui::TouchPhase::Started) => {
                self.scroll_px = 0.;
                None
            }
            /* Calculate the appropriate scroll lines */
            Some(gpui::TouchPhase::Moved) => {
                let old_offset = (self.scroll_px / self.cur_size.line_height) as i32;

                self.scroll_px += e.delta.y() * scroll_multiplier;

                let new_offset = (self.scroll_px / self.cur_size.line_height) as i32;

                // Whenever we hit the edges, reset our stored scroll to 0
                // so we can respond to changes in direction quickly
                self.scroll_px %= self.cur_size.height;

                Some(new_offset - old_offset)
            }
            /* Fall back to delta / line_height */
            None => Some(((e.delta.y() * scroll_multiplier) / self.cur_size.line_height) as i32),
            _ => None,
        }
    }

    pub fn find_matches(
        &mut self,
        query: project::search::SearchQuery,
        cx: &mut ModelContext<Self>,
    ) -> Task<Vec<RangeInclusive<Point>>> {
        let term = self.term.clone();
        cx.background().spawn(async move {
            let searcher = match query {
                project::search::SearchQuery::Text { query, .. } => {
                    RegexSearch::new(query.as_ref())
                }
                project::search::SearchQuery::Regex { query, .. } => {
                    RegexSearch::new(query.as_ref())
                }
            };

            if searcher.is_err() {
                return Vec::new();
            }
            let searcher = searcher.unwrap();

            let term = term.lock();

            make_search_matches(&term, &searcher).collect()
        })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.pty_tx.0.send(Msg::Shutdown).ok();
    }
}

impl Entity for Terminal {
    type Event = Event;
}

fn make_selection(range: &RangeInclusive<Point>) -> Selection {
    let mut selection = Selection::new(SelectionType::Simple, *range.start(), AlacDirection::Left);
    selection.update(*range.end(), AlacDirection::Right);
    selection
}

/// Copied from alacritty/src/display/hint.rs HintMatches::visible_regex_matches()
/// Iterate over all visible regex matches.
fn make_search_matches<'a, T>(
    term: &'a Term<T>,
    regex: &'a RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - MAX_SEARCH_LINES);
    end.line = end.line.min(viewport_end + MAX_SEARCH_LINES);

    RegexIter::new(start, end, AlacDirection::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

#[cfg(test)]
mod tests {
    use libc::c_int;

    pub mod terminal_test_context;

    #[test]
    pub fn wez_test() {
        fn test() -> Option<Vec<String>> {
            let size = 28;

            //Test data pulled from running the code
            let buf = [
                2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 0, 0, 0, 0, 0, 115,
                108, 101, 101, 112, 0, 53, 0,
            ];

            let mut ptr = &buf[0..size];

            let argc: c_int = unsafe { std::ptr::read(ptr.as_ptr() as *const c_int) };
            ptr = &ptr[std::mem::size_of::<c_int>()..];

            fn consume_cstr(ptr: &mut &[u8]) -> Option<String> {
                let nul = ptr.iter().position(|&c| c == 0)?;
                let s = String::from_utf8_lossy(&ptr[0..nul]).to_owned().to_string();
                *ptr = ptr.get(nul + 1..)?;
                Some(s)
            }

            let _exe_path: Option<String> = consume_cstr(&mut ptr)?.into();

            //Clear out the trailing null pointers
            while ptr[0] == 0 {
                ptr = ptr.get(1..)?;
            }

            let mut args = vec![];
            for _ in 0..argc {
                args.push(consume_cstr(&mut ptr)?);
            }
            Some(args)
        }

        assert_eq!(test(), Some(vec!["sleep".to_string(), "5".to_string()]));
    }
}

mod wez_proc_info {}
