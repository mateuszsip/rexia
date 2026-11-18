//! One terminal's state on the client: the Ghostty terminal restored from the stream's
//! snapshot, Rex metadata applied in order, and immutable frames for the UI to draw.
//! Selection, search, scrollback and prompt navigation run here, where the terminal lives.
use super::data::{ApcParser, Part, SnapshotBuffer};
use super::images::{self, ImageCache, Placement};
use anyhow::{Context, Result, ensure};
use ghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, CursorVisualStyle, ImageIterator, RowIterator},
    screen::RowSemanticPrompt,
    search::Search,
    selection::{FormatOptions, SelectLineOptions, SelectWordOptions, Selection},
    style::{RgbColor, Underline},
    terminal::{Point, PointCoordinate, PointSpace, ScrollViewport},
};
use serde_json::Value;

pub mod flags {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 1 << 1;
    pub const FAINT: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 4;
    pub const CURLY_UNDERLINE: u16 = 1 << 5;
    pub const STRIKE: u16 = 1 << 6;
    pub const OVERLINE: u16 = 1 << 7;
    pub const BLINK: u16 = 1 << 8;
    pub const SELECTED: u16 = 1 << 9;
    pub const MATCH: u16 = 1 << 10;
    pub const CURRENT_MATCH: u16 = 1 << 11;
    /// The first cell of a double-width character.
    pub const WIDE: u16 = 1 << 12;
    /// The cell a double-width character extends over; draw nothing in it.
    pub const SPACER: u16 = 1 << 13;
    pub const LINK: u16 = 1 << 14;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub fg: u32,
    /// None when the cell uses the terminal's default background.
    pub bg: Option<u32>,
    pub flags: u16,
}

impl Cell {
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub blinking: bool,
    pub color: Option<u32>,
    pub wide: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub visible: u64,
}

impl Scrollbar {
    pub fn at_bottom(&self) -> bool {
        self.offset + self.visible >= self.total
    }
}

/// Everything the UI needs to draw one terminal at one moment.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Cell>>,
    pub fg: u32,
    pub bg: u32,
    pub cursor: Option<Cursor>,
    pub mouse_tracking: bool,
    /// The program asked for bracketed paste (mode 2004), so a paste can't run anything.
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    pub scrollbar: Scrollbar,
    pub has_selection: bool,
    /// Match count and the selected match's index, while a search is open.
    pub search: Option<(usize, Option<usize>)>,
    /// The 16 ANSI colors, for chrome that matches the terminal.
    pub ansi: [u32; 16],
    /// Kitty graphics on screen, in drawing order.
    pub images: Vec<Placement>,
    /// The terminal's cell size in pixels, which image offsets and sizes are measured in.
    pub cell_px: (u32, u32),
    /// Which ways of sending images the terminal takes, besides directly.
    pub mediums: images::Mediums,
}

impl Frame {
    /// The plain text of one viewport row, without padding of wide characters.
    pub fn line_text(&self, row: usize) -> String {
        self.lines
            .get(row)
            .map(|cells| {
                cells
                    .iter()
                    .filter(|c| !c.has(flags::SPACER))
                    .map(|c| {
                        if c.text.is_empty() {
                            " "
                        } else {
                            c.text.as_str()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .unwrap_or_default()
    }
}

/// Things programs did that the client may act on.
#[derive(Clone, Debug, PartialEq)]
pub enum TermEvent {
    Bell,
    Title(String),
    Pwd(String),
    Notification { title: String, body: String },
    Clipboard(String),
    ChildExited(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    Cell,
    Word,
    Line,
    Rectangle,
}

pub fn rgb(color: RgbColor) -> u32 {
    (color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32
}

fn color(value: Option<&str>) -> Option<RgbColor> {
    let hex = value?.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(RgbColor {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    })
}

/// A terminal theme the client asks the server to apply (`set_theme`), or applies locally.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalTheme {
    pub foreground: u32,
    pub background: u32,
    pub cursor: Option<u32>,
    pub palette: Vec<(u8, u32)>,
}

pub struct TerminalState {
    // Declared before `terminal` so it's dropped first: it points into the terminal.
    search: Option<Search>,
    terminal: Option<Terminal>,
    snapshot: SnapshotBuffer,
    parser: ApcParser,
    render: RenderState,
    rows: RowIterator,
    cells: CellIterator,
    image_list: ImageIterator,
    image_cache: ImageCache,
    /// Whether programs run on this machine, so image files they name can be read.
    local: bool,
    /// Which ways of sending images this terminal takes, besides directly.
    mediums: images::Mediums,
    /// The size this client last reflowed to.
    local_size: Option<(u16, u16, u16, u16)>,
    /// The clock Kitty animations play on, and when their next frame is due on it.
    clock: std::time::Instant,
    clock_skip_ms: u64,
    next_animation_ms: Option<u64>,
    /// Bytes received since the stream started, for resuming at the same offset.
    pub received: u64,
    pub exit_code: Option<i64>,
    events: Vec<TermEvent>,
    /// The selection's fixed end, in screen coordinates (scrollback included).
    anchor: Option<(PointCoordinate, SelectMode)>,
    local_theme: Option<TerminalTheme>,
    /// The cursor programs get until they choose one (and after they reset it).
    default_cursor: Option<CursorShape>,
}

fn ghostty_cursor(shape: CursorShape) -> ghostty_vt::terminal::CursorStyle {
    use ghostty_vt::terminal::CursorStyle;
    match shape {
        CursorShape::Block => CursorStyle::Block,
        CursorShape::HollowBlock => CursorStyle::BlockHollow,
        CursorShape::Bar => CursorStyle::Bar,
        CursorShape::Underline => CursorStyle::Underline,
    }
}

impl TerminalState {
    pub fn new(nonce: u64) -> Result<Self> {
        Ok(Self {
            search: None,
            terminal: None,
            snapshot: SnapshotBuffer::default(),
            parser: ApcParser::new(nonce),
            render: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            image_list: ImageIterator::new().context("kitty image list")?,
            image_cache: ImageCache::default(),
            local: false,
            mediums: images::Mediums::default(),
            local_size: None,
            clock: std::time::Instant::now(),
            clock_skip_ms: 0,
            next_animation_ms: None,
            received: 0,
            exit_code: None,
            events: Vec::new(),
            anchor: None,
            local_theme: None,
            default_cursor: None,
        })
    }

    /// Whether the server runs on this machine: image files programs name can be read.
    pub fn set_local(&mut self, local: bool) -> Result<()> {
        self.local = local;
        if let Some(terminal) = self.terminal.as_mut() {
            self.mediums = images::enable(terminal, local).context("enabling kitty graphics")?;
        }
        Ok(())
    }

    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64 + self.clock_skip_ms
    }

    /// How long until a playing Kitty animation shows its next frame.
    pub fn animation_wait(&self) -> Option<std::time::Duration> {
        let next = self.next_animation_ms?;
        Some(std::time::Duration::from_millis(
            next.saturating_sub(self.now_ms()),
        ))
    }

    /// Move the animation clock forward, for tests.
    #[cfg(test)]
    fn skip_ms(&mut self, ms: u64) {
        self.clock_skip_ms += ms;
    }

    pub fn ready(&self) -> bool {
        self.terminal.is_some()
    }

    pub fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    pub fn take_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events)
    }

    /// Feed bytes from the data channel. The first bytes complete the snapshot.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.received += bytes.len() as u64;
        let tail;
        let bytes = if self.terminal.is_none() {
            let Some((snapshot, rest)) = self.snapshot.push(bytes)? else {
                return Ok(());
            };
            let mut decoder = ghostty_vt::snapshot::Decoder::from_slice(&snapshot);
            // Images on screen when this client attached come back with the snapshot.
            decoder.set_kitty_image_storage_limit(images::STORAGE_LIMIT)?;
            decoder.ready()?;
            while decoder.next_page()?.is_some() {}
            ensure!(
                decoder.source_offset()? == Some(snapshot.len()),
                "terminal snapshot boundary mismatch"
            );
            let mut terminal = decoder
                .into_terminal()
                .context("terminal snapshot has no terminal")?;
            self.mediums =
                images::enable(&mut terminal, self.local).context("enabling kitty graphics")?;
            self.terminal = Some(terminal);
            if let Some(theme) = self.local_theme.clone() {
                self.apply_theme(&theme)?;
            }
            // A steady block is also the server's default, so it takes this client's.
            self.apply_default_cursor(None)?;
            if let Some((cols, rows, cell_width, cell_height)) = self.local_size {
                self.reflow(cols, rows, cell_width, cell_height)?;
            }
            tail = rest;
            &tail[..]
        } else {
            bytes
        };
        images::install_png_decoder();
        for part in self.parser.feed(bytes)? {
            match part {
                Part::Output(output) => {
                    let terminal = self.terminal.as_mut().expect("snapshot restored");
                    terminal.vt_write(&output);
                }
                Part::Metadata(value) => self.metadata(&value)?,
            }
        }
        self.collect_events();
        if let (Some(search), Some(terminal)) = (self.search.as_mut(), self.terminal.as_ref()) {
            search.feed(terminal)?;
            search.run(terminal)?;
        }
        Ok(())
    }

    fn metadata(&mut self, value: &Value) -> Result<()> {
        let terminal = self.terminal.as_mut().expect("snapshot restored");
        match value["type"].as_str() {
            Some("resize") => {
                let dimension = |key: &str| {
                    value[key]
                        .as_u64()
                        .filter(|v| (1..=65535).contains(v))
                        .map(|v| v as u16)
                        .with_context(|| format!("invalid terminal resize {key}"))
                };
                terminal.resize(
                    dimension("cols")?,
                    dimension("rows")?,
                    value["cell_width_px"].as_u64().unwrap_or(0) as u32,
                    value["cell_height_px"].as_u64().unwrap_or(0) as u32,
                )?;
            }
            Some("theme") => {
                // A local theme wins over the server's: this client chose its colors.
                if self.local_theme.is_none() {
                    terminal.set_default_fg_color(color(value["foreground"].as_str()))?;
                    terminal.set_default_bg_color(color(value["background"].as_str()))?;
                    terminal.set_default_cursor_color(color(value["cursor"].as_str()))?;
                    terminal.set_default_color_palette(None)?;
                    if let Some(entries) = value["palette"].as_array() {
                        let mut palette = terminal.color_palette()?;
                        for entry in entries.iter().filter_map(Value::as_str) {
                            if let Some((index, hex)) = entry.split_once('=')
                                && let (Ok(index), Some(rgb)) =
                                    (index.parse::<u8>(), color(Some(hex)))
                            {
                                palette.0[index as usize] = rgb;
                            }
                        }
                        terminal.set_default_color_palette(Some(palette))?;
                    }
                }
            }
            Some("child_exited") => {
                self.exit_code = value["exit_code"].as_i64();
                if let Some(code) = self.exit_code {
                    self.events.push(TermEvent::ChildExited(code));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn collect_events(&mut self) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        for event in terminal.events() {
            use ghostty_vt::terminal::Event;
            let event = match event {
                Event::Bell => TermEvent::Bell,
                Event::TitleChanged(title) => TermEvent::Title(title),
                Event::PwdChanged(pwd) => TermEvent::Pwd(pwd),
                Event::DesktopNotification { title, body } => {
                    TermEvent::Notification { title, body }
                }
                Event::ClipboardWrite(write) => {
                    let Some(text) = write
                        .contents
                        .iter()
                        .find(|c| c.mime.starts_with("text/plain"))
                    else {
                        continue;
                    };
                    TermEvent::Clipboard(String::from_utf8_lossy(&text.data).into_owned())
                }
                _ => continue,
            };
            if self.events.len() < 64 {
                self.events.push(event);
            }
        }
    }

    /// The cursor shape programs get by default. A program's own choice stays until it
    /// resets the cursor; a cursor still showing the old default changes now.
    pub fn set_default_cursor(&mut self, shape: Option<CursorShape>) -> Result<()> {
        let previous = std::mem::replace(&mut self.default_cursor, shape);
        self.apply_default_cursor(previous)
    }

    fn apply_default_cursor(&mut self, previous: Option<CursorShape>) -> Result<()> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };
        terminal.set_default_cursor_style(self.default_cursor.map(ghostty_cursor))?;
        let snapshot = self.render.update(terminal)?;
        let current = match snapshot.cursor_visual_style() {
            Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
            Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
            Ok(CursorVisualStyle::BlockHollow) => CursorShape::HollowBlock,
            _ => CursorShape::Block,
        };
        let steady = !snapshot.cursor_blinking().unwrap_or(false);
        if steady && current == previous.unwrap_or(CursorShape::Block) {
            // DECSCUSR 0: back to the default, which is now this one.
            terminal.vt_write(b"\x1b[0 q");
        }
        Ok(())
    }

    /// Use these colors instead of the server's (the user picked a local theme).
    pub fn set_local_theme(&mut self, theme: Option<TerminalTheme>) -> Result<()> {
        self.local_theme = theme.clone();
        if let Some(theme) = theme {
            self.apply_theme(&theme)?;
        }
        Ok(())
    }

    fn apply_theme(&mut self, theme: &TerminalTheme) -> Result<()> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };
        let to_rgb = |v: u32| RgbColor {
            r: (v >> 16) as u8,
            g: (v >> 8) as u8,
            b: v as u8,
        };
        terminal.set_default_fg_color(Some(to_rgb(theme.foreground)))?;
        terminal.set_default_bg_color(Some(to_rgb(theme.background)))?;
        terminal.set_default_cursor_color(theme.cursor.map(to_rgb))?;
        terminal.set_default_color_palette(None)?;
        let mut palette = terminal.color_palette()?;
        for (index, value) in &theme.palette {
            palette.0[*index as usize] = to_rgb(*value);
        }
        terminal.set_default_color_palette(Some(palette))?;
        Ok(())
    }

    pub fn scroll(&mut self, scroll: ScrollViewport) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.scroll_viewport(scroll);
        }
    }

    /// Move the viewport to the previous (`-1`) or next (`1`) shell prompt (OSC 133).
    /// Reflow the terminal to `cols` by `rows` here, before the server's own resize
    /// arrives; that one finds it already this size.
    pub fn reflow(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        // A size asked for before the snapshot arrives applies once it has.
        self.local_size = Some((cols, rows, cell_width, cell_height));
        if let Some(terminal) = self.terminal.as_mut()
            && (terminal.cols()?, terminal.rows()?) != (cols, rows)
        {
            terminal.resize(cols, rows, u32::from(cell_width), u32::from(cell_height))?;
        }
        Ok(())
    }

    pub fn jump_to_prompt(&mut self, direction: i32) -> Result<bool> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(false);
        };
        let bar = terminal.scrollbar()?;
        let top = bar.offset as i64;
        let last = bar.total.saturating_sub(1) as i64;
        let mut y = top + direction as i64;
        // Bounded so a huge scrollback can't stall the UI.
        for _ in 0..50_000 {
            if y < 0 || y > last {
                return Ok(false);
            }
            let point = Point::Screen(PointCoordinate { x: 0, y: y as u32 });
            if let Ok(grid) = terminal.grid_ref(point)
                && grid.row()?.semantic_prompt()? == RowSemanticPrompt::Prompt
            {
                terminal.scroll_viewport(ScrollViewport::Row(y as usize));
                return Ok(true);
            }
            y += direction as i64;
        }
        Ok(false)
    }

    fn screen_point(terminal: &Terminal, x: u16, y: u16) -> Result<PointCoordinate> {
        let offset = terminal.scrollbar()?.offset;
        Ok(PointCoordinate {
            x,
            y: (offset + y as u64) as u32,
        })
    }

    /// Start a selection at a viewport cell.
    pub fn select_start(&mut self, x: u16, y: u16, mode: SelectMode) -> Result<()> {
        let Some(terminal) = self.terminal.as_ref() else {
            return Ok(());
        };
        let point = Self::screen_point(terminal, x, y)?;
        self.anchor = Some((point, mode));
        let grid = terminal.grid_ref(Point::Screen(point))?;
        let selection = match mode {
            SelectMode::Word => terminal.select_word(SelectWordOptions::new(grid))?,
            SelectMode::Line => terminal.select_line(SelectLineOptions::new(grid))?,
            SelectMode::Cell | SelectMode::Rectangle => None,
        };
        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    /// Extend the selection from its anchor to a viewport cell.
    pub fn select_to(&mut self, x: u16, y: u16) -> Result<()> {
        let (Some(terminal), Some((anchor, mode))) = (self.terminal.as_ref(), self.anchor) else {
            return Ok(());
        };
        let head = Self::screen_point(terminal, x, y)?;
        let start = terminal.grid_ref(Point::Screen(anchor))?;
        let end = terminal.grid_ref(Point::Screen(head))?;
        let selection = match mode {
            SelectMode::Cell => Selection::new(start, end, false),
            SelectMode::Rectangle => Selection::new(start, end, true),
            SelectMode::Word | SelectMode::Line => {
                let (first, second) = if (anchor.y, anchor.x) <= (head.y, head.x) {
                    (start, end)
                } else {
                    (end, start)
                };
                let unit = |grid| {
                    if mode == SelectMode::Word {
                        terminal.select_word(SelectWordOptions::new(grid))
                    } else {
                        terminal.select_line(SelectLineOptions::new(grid))
                    }
                };
                let (first_point, second_point) = if (anchor.y, anchor.x) <= (head.y, head.x) {
                    (anchor, head)
                } else {
                    (head, anchor)
                };
                match (unit(first)?, unit(second)?) {
                    (Some(a), Some(b)) => Selection::new(a.start(), b.end(), false),
                    (Some(a), None) | (None, Some(a)) => a,
                    (None, None) => Selection::new(
                        terminal.grid_ref(Point::Screen(first_point))?,
                        terminal.grid_ref(Point::Screen(second_point))?,
                        false,
                    ),
                }
            }
        };
        terminal.set_selection(Some(&selection))?;
        Ok(())
    }

    pub fn select_all(&mut self) -> Result<()> {
        if let Some(terminal) = self.terminal.as_ref() {
            let all = terminal.select_all()?;
            terminal.set_selection(all.as_ref())?;
            self.anchor = None;
        }
        Ok(())
    }

    pub fn clear_selection(&mut self) -> Result<()> {
        self.anchor = None;
        if let Some(terminal) = self.terminal.as_ref() {
            terminal.set_selection(None)?;
        }
        Ok(())
    }

    /// The selected text, unwrapped and with trailing spaces trimmed.
    pub fn selection_text(&self) -> Result<Option<String>> {
        let Some(terminal) = self.terminal.as_ref() else {
            return Ok(None);
        };
        let Some(selection) = terminal.selection()? else {
            return Ok(None);
        };
        let options = FormatOptions::new()
            .with_selection(&selection)
            .with_unwrap(true)
            .with_trim(true);
        Ok(terminal
            .format_selection_alloc(options)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// The screen and scrollback as plain text.
    pub fn text(&self) -> Result<String> {
        let terminal = self
            .terminal
            .as_ref()
            .context("terminal not restored yet")?;
        let mut formatter = ghostty_vt::fmt::Formatter::new(
            terminal,
            ghostty_vt::fmt::FormatterOptions::new()
                .with_unwrap(true)
                .with_trim(true),
        )?;
        Ok(String::from_utf8_lossy(&formatter.format_alloc()?).into_owned())
    }

    /// The OSC 8 link at a viewport cell.
    pub fn link_at(&self, x: u16, y: u16) -> Option<String> {
        let terminal = self.terminal.as_ref()?;
        let grid = terminal
            .grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))
            .ok()?;
        let mut buffer = vec![0; 4096];
        let len = grid.hyperlink_uri(&mut buffer).ok()?;
        (len > 0).then(|| String::from_utf8_lossy(&buffer[..len.min(buffer.len())]).into_owned())
    }

    /// Search the screen and scrollback; `None` closes the search.
    pub fn search(&mut self, needle: Option<&str>) -> Result<()> {
        let Some(terminal) = self.terminal.as_ref() else {
            return Ok(());
        };
        match needle.filter(|n| !n.is_empty()) {
            None => self.search = None,
            Some(needle) => {
                if self.search.is_none() {
                    self.search = Some(Search::new(terminal)?);
                }
                let search = self.search.as_mut().expect("search");
                search.set_needle(terminal, Some(needle))?;
                search.feed(terminal)?;
                search.run(terminal)?;
                search.select_prev(terminal)?;
            }
        }
        Ok(())
    }

    /// Select the next older (`true`) or newer match, scrolling to it.
    pub fn search_step(&mut self, older: bool) -> Result<()> {
        if let (Some(search), Some(terminal)) = (self.search.as_mut(), self.terminal.as_ref()) {
            if older {
                search.select_next(terminal)?;
            } else {
                search.select_prev(terminal)?;
            }
        }
        Ok(())
    }

    pub fn frame(&mut self) -> Result<Option<Frame>> {
        let now = self.now_ms();
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(None);
        };
        // Playing animations move to the frame due now.
        self.next_animation_ms = terminal.kitty_animation_tick(now).unwrap_or(None);
        let terminal = &*terminal;
        let mut matches: Vec<(u32, u16, u32, u16, bool)> = Vec::new();
        let mut search_state = None;
        if let Some(search) = self.search.as_ref() {
            let current = search.selected_match(terminal)?;
            let mut push = |selection: &Selection<'_>, current: bool| -> Result<()> {
                let start =
                    terminal.point_from_grid_ref(&selection.start(), PointSpace::Viewport)?;
                let end = terminal.point_from_grid_ref(&selection.end(), PointSpace::Viewport)?;
                if let (Some(a), Some(b)) = (start, end) {
                    matches.push((a.y, a.x, b.y, b.x, current));
                }
                Ok(())
            };
            for selection in search.viewport_matches(terminal)? {
                push(&selection, false)?;
            }
            if let Some(selection) = current {
                push(&selection, true)?;
            }
            search_state = Some((search.total_matches()?, search.selected_index()?));
        }

        let snapshot = self.render.update(terminal)?;
        let colors = snapshot.colors()?;
        let fg = rgb(colors.foreground);
        let bg = rgb(colors.background);
        let mut ansi = [0; 16];
        for (index, slot) in ansi.iter_mut().enumerate() {
            *slot = rgb(colors.palette[index]);
        }
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|at| Cursor {
                x: at.x,
                y: at.y,
                shape: match snapshot.cursor_visual_style() {
                    Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                    Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                    Ok(CursorVisualStyle::BlockHollow) => CursorShape::HollowBlock,
                    _ => CursorShape::Block,
                },
                blinking: snapshot.cursor_blinking().unwrap_or(false),
                color: snapshot.cursor_color().ok().flatten().map(rgb),
                wide: at.at_wide_tail,
            })
        } else {
            None
        };
        let bar = terminal.scrollbar()?;
        let mut frame = Frame {
            cols: snapshot.cols()?,
            rows: snapshot.rows()?,
            lines: Vec::new(),
            fg,
            bg,
            cursor,
            mouse_tracking: terminal.is_mouse_tracking()?,
            bracketed_paste: terminal.mode(ghostty_vt::terminal::Mode::BRACKETED_PASTE)?,
            alternate_screen: terminal.active_screen()? == ghostty_vt::screen::Screen::Alternate,
            scrollbar: Scrollbar {
                total: bar.total,
                offset: bar.offset,
                visible: bar.len,
            },
            has_selection: terminal.selection()?.is_some(),
            search: search_state,
            ansi,
            images: Vec::new(),
            mediums: self.mediums,
            cell_px: {
                let (cols, rows) = (terminal.cols()?.max(1), terminal.rows()?.max(1));
                (
                    terminal.width_px()? / u32::from(cols),
                    terminal.height_px()? / u32::from(rows),
                )
            },
        };

        let mut rows = self.rows.update(&snapshot)?;
        let mut y: u32 = 0;
        while let Some(row) = rows.next() {
            let selected = row.selection()?;
            let mut cells = self.cells.update(row)?;
            let mut line = Vec::with_capacity(frame.cols as usize);
            let mut x: u16 = 0;
            while let Some(cell) = cells.next() {
                let mut text = String::new();
                cell.graphemes_utf8(&mut text)?;
                // A Unicode placeholder shows part of an image, drawn from the image list.
                if text.starts_with(images::PLACEHOLDER) {
                    text.clear();
                }
                let style = cell.style()?;
                let raw = cell.raw_cell()?;
                let mut cell_fg = cell.fg_color()?.map(rgb).unwrap_or(fg);
                let mut cell_bg = cell.bg_color()?.map(rgb);
                if style.inverse {
                    let back = cell_bg.unwrap_or(bg);
                    cell_bg = Some(cell_fg);
                    cell_fg = back;
                }
                if style.invisible {
                    text.clear();
                }
                let mut bits = [
                    (style.bold, flags::BOLD),
                    (style.italic, flags::ITALIC),
                    (style.faint, flags::FAINT),
                    (style.strikethrough, flags::STRIKE),
                    (style.overline, flags::OVERLINE),
                    (style.blink, flags::BLINK),
                    (raw.has_hyperlink().unwrap_or(false), flags::LINK),
                ]
                .iter()
                .filter(|(on, _)| *on)
                .fold(0, |bits, (_, flag)| bits | flag);
                match style.underline {
                    Underline::None => {}
                    Underline::Double => bits |= flags::DOUBLE_UNDERLINE,
                    Underline::Curly => bits |= flags::CURLY_UNDERLINE,
                    _ => bits |= flags::UNDERLINE,
                }
                match raw.wide()? {
                    ghostty_vt::screen::CellWide::Wide => bits |= flags::WIDE,
                    ghostty_vt::screen::CellWide::SpacerTail
                    | ghostty_vt::screen::CellWide::SpacerHead => bits |= flags::SPACER,
                    _ => {}
                }
                if let Some(range) = selected
                    && x >= range.start_x
                    && x <= range.end_x
                {
                    bits |= flags::SELECTED;
                }
                for &(sy, sx, ey, ex, current) in &matches {
                    let after_start = y > sy || (y == sy && x >= sx);
                    let before_end = y < ey || (y == ey && x <= ex);
                    if after_start && before_end {
                        bits |= if current {
                            flags::CURRENT_MATCH
                        } else {
                            flags::MATCH
                        };
                    }
                }
                line.push(Cell {
                    text,
                    fg: cell_fg,
                    bg: cell_bg,
                    flags: bits,
                });
                x += 1;
            }
            frame.lines.push(line);
            y += 1;
        }
        frame.images = self
            .image_cache
            .placements(&snapshot, &mut self.image_list, terminal)
            .unwrap_or_default();
        snapshot.clean()?;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::images::Layer;
    use ghostty_vt::TerminalOptions;

    fn stream_from(write: &[u8], cols: u16, rows: u16) -> TerminalState {
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            ..Default::default()
        })
        .unwrap();
        terminal.vt_write(write);
        let snapshot = ghostty_vt::snapshot::encode(&terminal).unwrap();
        let mut state = TerminalState::new(42).unwrap();
        state.feed(&snapshot).unwrap();
        state
    }

    /// A Kitty graphics command, as a program sends it.
    fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(payload);
        format!("\x1b_G{control};{data}\x1b\\").into_bytes()
    }

    /// A terminal of 20 by 6 cells of 10 by 20 pixels, as a resize from Rex sets it.
    fn graphics_stream() -> TerminalState {
        let mut state = stream_from(b"", 20, 6);
        state
            .feed(
                concat!(
                    "\x1b_rex1;42;",
                    r#"{"type":"resize","cols":20,"rows":6,"cell_width_px":10,"cell_height_px":20}"#,
                    "\x1b\\"
                )
                .as_bytes(),
            )
            .unwrap();
        state
    }

    fn images_of(state: &mut TerminalState) -> Vec<Placement> {
        state.frame().unwrap().unwrap().images
    }

    #[test]
    fn images_on_screen_come_back_when_attaching() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 6,
            ..Default::default()
        })
        .unwrap();
        terminal.resize(20, 6, 10, 20).unwrap();
        terminal.set_kitty_image_storage_limit(1 << 20).unwrap();
        terminal.vt_write(&kitty("a=T,f=24,s=1,v=1,i=3,q=2", &[1, 2, 3]));
        let snapshot = ghostty_vt::snapshot::encode(&terminal).unwrap();
        let mut state = TerminalState::new(42).unwrap();
        state.feed(&snapshot).unwrap();
        let images = images_of(&mut state);
        assert_eq!(images.len(), 1, "the image came back with the snapshot");
        assert_eq!(images[0].pixels.rgba, vec![1, 2, 3, 255]);
    }

    #[test]
    fn kitty_animations_play_on_their_clock() {
        let mut state = graphics_stream();
        // One red pixel, then a blue second frame; each shows for 100 ms, looping.
        state
            .feed(&kitty("a=T,f=24,s=1,v=1,i=4,q=2", &[255, 0, 0]))
            .unwrap();
        state
            .feed(&kitty("a=f,f=24,s=1,v=1,i=4,z=100,q=2", &[0, 0, 255]))
            .unwrap();
        state.feed(&kitty("a=a,i=4,r=1,z=100,q=2", &[])).unwrap();
        state.feed(&kitty("a=a,i=4,s=3,v=1,q=2", &[])).unwrap();
        let pixel = |state: &mut TerminalState| images_of(state)[0].pixels.rgba[..3].to_vec();
        assert_eq!(pixel(&mut state), [255, 0, 0]);
        assert!(state.animation_wait().is_some(), "a frame is due");
        state.skip_ms(150);
        assert_eq!(pixel(&mut state), [0, 0, 255], "the second frame shows");
        state.skip_ms(150);
        assert_eq!(pixel(&mut state), [255, 0, 0], "and loops back");
        // Stopped, nothing more is due.
        state.feed(&kitty("a=a,i=4,s=1,q=2", &[])).unwrap();
        let _ = state.frame().unwrap();
        assert_eq!(state.animation_wait(), None);
    }

    #[test]
    fn image_files_are_read_only_for_local_servers() {
        let mut state = graphics_stream();
        let remote = state.frame().unwrap().unwrap().mediums;
        state.set_local(true).unwrap();
        let local = state.frame().unwrap().unwrap().mediums;
        println!("remote {remote:?}, local {local:?}");
        assert!(!remote.file && !remote.shared_memory);
        assert!(local.file && local.shared_memory);
    }

    #[test]
    fn reflowing_rewraps_text_before_the_server_resizes() {
        let mut state = stream_from(b"0123456789abcdefghij", 20, 4);
        assert_eq!(
            state.frame().unwrap().unwrap().line_text(0).trim_end(),
            "0123456789abcdefghij"
        );
        state.reflow(10, 4, 10, 20).unwrap();
        let frame = state.frame().unwrap().unwrap();
        assert_eq!(frame.cols, 10);
        assert_eq!(frame.line_text(0), "0123456789");
        assert_eq!(frame.line_text(1).trim_end(), "abcdefghij");
        // The server's resize to the same size changes nothing.
        state
            .feed(
                concat!(
                    "\x1b_rex1;42;",
                    r#"{"type":"resize","cols":10,"rows":4,"cell_width_px":10,"cell_height_px":20}"#,
                    "\x1b\\"
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(
            state.frame().unwrap().unwrap().line_text(1).trim_end(),
            "abcdefghij"
        );
    }

    #[test]
    fn kitty_images_show_in_frames_by_layer() {
        let mut state = graphics_stream();
        assert_eq!(state.frame().unwrap().unwrap().cell_px, (10, 20));
        // Two red RGB pixels and two blue, sent directly and shown at the cursor.
        let rgb = [255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255];
        state
            .feed(&kitty("a=T,f=24,s=2,v=2,i=1,q=2", &rgb))
            .unwrap();
        let images = images_of(&mut state);
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!(
            (image.pixels.id, image.pixels.width, image.pixels.height),
            (1, 2, 2)
        );
        assert_eq!(image.pixels.rgba[..4], [255, 0, 0, 255]);
        assert_eq!(image.pixels.rgba[8..12], [0, 0, 255, 255]);
        assert_eq!(
            (image.col, image.row, image.layer),
            (0, 0, Layer::AboveText)
        );
        assert_eq!((image.width, image.height), (2, 2));
        // Scaled to cells, under the text, and under the backgrounds.
        state.feed(b"\r\n\r\n").unwrap();
        state.feed(&kitty("a=p,i=1,c=3,r=2,z=-1,q=2", &[])).unwrap();
        state
            .feed(&kitty("a=p,i=1,p=9,z=-1073741825,q=2", &[]))
            .unwrap();
        let images = images_of(&mut state);
        let layers: Vec<_> = images
            .iter()
            .map(|p| (p.layer, p.width, p.height))
            .collect();
        assert_eq!(
            layers,
            vec![
                (Layer::BelowBackground, 2, 2),
                (Layer::BelowText, 30, 40),
                (Layer::AboveText, 2, 2),
            ]
        );
        // A placement attached to another moves with it: 3 cells right, 1 down.
        let parent = images
            .iter()
            .find(|p| p.layer == Layer::BelowBackground)
            .map(|p| (p.col, p.row))
            .unwrap();
        state
            .feed(&kitty("a=p,i=1,p=10,P=1,Q=9,H=3,V=1,q=2", &[]))
            .unwrap();
        let images = images_of(&mut state);
        assert_eq!(images.len(), 4);
        assert!(
            images
                .iter()
                .any(|p| (p.col, p.row) == (parent.0 + 3, parent.1 + 1)),
            "relative placement at {parent:?} + (3, 1): {:?}",
            images.iter().map(|p| (p.col, p.row)).collect::<Vec<_>>()
        );
        // Deleting every placement clears them all.
        state.feed(&kitty("a=d,d=A,q=2", &[])).unwrap();
        assert!(images_of(&mut state).is_empty());
    }

    #[test]
    fn kitty_png_images_and_unicode_placeholders() {
        let mut state = graphics_stream();
        // A 3 by 1 PNG.
        let mut png = Vec::new();
        image::RgbaImage::from_raw(3, 1, vec![0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 128])
            .unwrap()
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        state.feed(&kitty("a=T,f=100,i=5,q=2", &png)).unwrap();
        let images = images_of(&mut state);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].pixels.width, images[0].pixels.height), (3, 1));
        assert_eq!(images[0].pixels.rgba[8..12], [0, 255, 0, 128]);
        state.feed(&kitty("a=d,d=A,q=2", &[])).unwrap();

        // A virtual placement, shown where a program prints placeholder cells: image 7
        // as the foreground color, then row 0 and columns 0 and 1 as diacritics.
        let rgb = [9u8; 2 * 2 * 3];
        state
            .feed(&kitty("a=T,U=1,f=24,s=2,v=2,i=7,c=2,r=1,q=2", &rgb))
            .unwrap();
        state
            .feed(
                "\r\n\x1b[38;5;7m\u{10EEEE}\u{305}\u{305}\u{10EEEE}\u{305}\u{30D}\x1b[39m"
                    .as_bytes(),
            )
            .unwrap();
        let frame = state.frame().unwrap().unwrap();
        let placed: Vec<_> = frame
            .images
            .iter()
            .map(|p| (p.pixels.id, p.col, p.row, p.layer))
            .collect();
        assert_eq!(placed, vec![(7, 0, 1, Layer::BelowText)]);
        assert_eq!(
            frame.line_text(1).trim(),
            "",
            "placeholders don't draw as text"
        );
    }

    fn cursor_of(state: &mut TerminalState) -> (CursorShape, bool) {
        let cursor = state.frame().unwrap().unwrap().cursor.unwrap();
        (cursor.shape, cursor.blinking)
    }

    #[test]
    fn default_cursor_applies_until_a_program_chooses() {
        // A pane whose program hasn't chosen a cursor takes the default, even mid-stream.
        let mut state = stream_from(b"$ ", 20, 4);
        state.set_default_cursor(Some(CursorShape::Bar)).unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Bar, false));
        // A program's choice wins (vim's steady block), and its reset brings the default back.
        state.feed(b"\x1b[2 q").unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Block, false));
        state
            .set_default_cursor(Some(CursorShape::Underline))
            .unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Block, false));
        state.feed(b"\x1b[0 q").unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Underline, false));
        // A blinking bar the program asked for is left alone.
        state.feed(b"\x1b[5 q").unwrap();
        state.set_default_cursor(None).unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Bar, true));
    }

    #[test]
    fn a_fresh_stream_starts_with_the_default_cursor() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 4,
            ..Default::default()
        })
        .unwrap();
        terminal.vt_write(b"$ ");
        let snapshot = ghostty_vt::snapshot::encode(&terminal).unwrap();
        let mut state = TerminalState::new(7).unwrap();
        state
            .set_default_cursor(Some(CursorShape::Underline))
            .unwrap();
        state.feed(&snapshot).unwrap();
        assert_eq!(cursor_of(&mut state), (CursorShape::Underline, false));
    }

    #[test]
    fn snapshot_then_output_then_metadata() {
        let mut state = stream_from(b"restored", 20, 4);
        state
            .feed(b"\r\n\x1b[1mlive\x1b[0m\x1b_rex1;42;{\"type\":\"resize\",\"cols\":30,\"rows\":5}\x1b\\")
            .unwrap();
        let frame = state.frame().unwrap().unwrap();
        assert_eq!((frame.cols, frame.rows), (30, 5));
        assert_eq!(frame.line_text(0), "restored");
        assert_eq!(frame.line_text(1), "live");
        assert!(frame.lines[1][0].has(flags::BOLD));
        assert!(!frame.lines[1][4].has(flags::BOLD));
    }

    #[test]
    fn wide_characters_and_colors() {
        let mut state = stream_from("界x\x1b[31mr\x1b[7mi".as_bytes(), 10, 2);
        let frame = state.frame().unwrap().unwrap();
        let line = &frame.lines[0];
        assert!(line[0].has(flags::WIDE));
        assert!(line[1].has(flags::SPACER));
        assert_eq!(line[2].text, "x");
        assert_ne!(line[3].fg, frame.fg, "red text");
        assert_eq!(
            line[4].bg,
            Some(line[3].fg),
            "inverse swaps red into the background"
        );
    }

    #[test]
    fn selection_copies_text_across_lines() {
        let mut state = stream_from(b"hello world\r\nsecond line", 20, 3);
        state.select_start(6, 0, SelectMode::Cell).unwrap();
        state.select_to(5, 1).unwrap();
        assert_eq!(state.selection_text().unwrap().unwrap(), "world\nsecond");
        let frame = state.frame().unwrap().unwrap();
        assert!(frame.has_selection);
        assert!(frame.lines[0][6].has(flags::SELECTED));
        assert!(!frame.lines[0][5].has(flags::SELECTED));
        state.select_start(1, 1, SelectMode::Word).unwrap();
        assert_eq!(state.selection_text().unwrap().unwrap(), "second");
        state.clear_selection().unwrap();
        assert!(state.selection_text().unwrap().is_none());
    }

    #[test]
    fn search_highlights_and_steps() {
        let mut state = stream_from(b"apple banana apple\r\ncherry apple", 30, 3);
        state.search(Some("apple")).unwrap();
        let frame = state.frame().unwrap().unwrap();
        assert_eq!(frame.search.unwrap().0, 3);
        let highlighted = frame
            .lines
            .iter()
            .flatten()
            .filter(|c| c.has(flags::MATCH) || c.has(flags::CURRENT_MATCH))
            .count();
        assert_eq!(highlighted, 15);
        let first = frame.search.unwrap().1;
        state.search_step(true).unwrap();
        assert_ne!(state.frame().unwrap().unwrap().search.unwrap().1, first);
        state.search(None).unwrap();
        assert!(state.frame().unwrap().unwrap().search.is_none());
    }

    #[test]
    fn scrollback_and_prompt_jumps() {
        let mut lines = Vec::new();
        for i in 0..40 {
            if i % 10 == 0 {
                lines.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n");
            } else {
                lines.extend_from_slice(format!("output {i}\r\n").as_bytes());
            }
        }
        let mut state = stream_from(&lines, 20, 5);
        let bottom = state.frame().unwrap().unwrap().scrollbar;
        assert!(bottom.at_bottom());
        state.scroll(ScrollViewport::Delta(-10));
        assert!(!state.frame().unwrap().unwrap().scrollbar.at_bottom());
        state.scroll(ScrollViewport::Bottom);
        assert!(state.jump_to_prompt(-1).unwrap());
        let frame = state.frame().unwrap().unwrap();
        assert!(
            frame.line_text(0).starts_with("$ cmd"),
            "{:?}",
            frame.line_text(0)
        );
    }

    #[test]
    fn program_events_are_collected() {
        let mut state = stream_from(b"", 10, 2);
        state
            .feed(b"\x07\x1b]0;my title\x07\x1b]9;done building\x07\x1b_rex1;42;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\")
            .unwrap();
        let events = state.take_events();
        assert!(events.contains(&TermEvent::Bell));
        assert!(events.contains(&TermEvent::Title("my title".into())));
        assert!(
            events.iter().any(
                |e| matches!(e, TermEvent::Notification { body, .. } if body == "done building")
            )
        );
        assert!(events.contains(&TermEvent::ChildExited(3)));
        assert_eq!(state.exit_code, Some(3));
    }

    #[test]
    fn osc52_copy_is_collected() {
        let mut state = stream_from(b"", 10, 2);
        state.feed(b"\x1b]52;c;aGVsbG8gd29ybGQ=\x1b\\").unwrap();
        assert_eq!(
            state.take_events(),
            vec![TermEvent::Clipboard("hello world".into())]
        );
    }

    #[test]
    fn local_theme_overrides_server_theme() {
        let mut state = stream_from(b"x", 10, 2);
        state
            .set_local_theme(Some(TerminalTheme {
                foreground: 0xe2e2e2,
                background: 0x000000,
                cursor: None,
                palette: vec![(1, 0xffb4ab)],
            }))
            .unwrap();
        state
            .feed(b"\x1b_rex1;42;{\"type\":\"theme\",\"foreground\":\"#111111\",\"background\":\"#ffffff\"}\x1b\\\x1b[31mr")
            .unwrap();
        let frame = state.frame().unwrap().unwrap();
        assert_eq!((frame.fg, frame.bg), (0xe2e2e2, 0x000000));
        assert_eq!(frame.lines[0][1].fg, 0xffb4ab);
    }
}
