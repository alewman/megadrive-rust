//! The VDP's picture: planes A and B, the window, sprites, priority, shadow/highlight.
//!
//! Transcribed from megadrive-python's `render.py`. A line at a time, drawn in pieces:
//! [`Renderer::begin_line`] is called by the VDP when a line's active display begins
//! (its HBlank-clear point), and reads what the line needs for its whole length (the
//! scroll values, the window's columns, and the sprites, fetched during the preceding
//! horizontal blank: Kabuto, phase 3). The pixels are drawn lazily: whenever the VDP is
//! about to change something the rest of the line would be drawn from, it calls
//! [`Renderer::catch_up`] and the line is drawn up to the pixel that change lands on.
//!
//! Sources (the reference ranks them): CMD `genvdp.txt` sections 13-15 and 17 for the
//! planes, scroll modes, sizes, the window and its column-fetch bug, and priority;
//! Kabuto's "S/H" for shadow/highlight; Plutiedev `vdp-color-ramp` for the colour
//! ramp; Kabuto's "Precise details of sprite rendering" and Nemesis's rules (forum
//! t=541 start=30) for sprites; Genesis Plus GX's plane-size tables (emulator-derived,
//! read and rewritten, never copied) for the invalid sizes.
//!
//! Rust's differences: the renderer is handed the VDP's state ([`VdpState`]) on each
//! call rather than holding the VDP; the reference's decoded-tile cache is a memo of a
//! pure function and is left out (a row is decoded where it is used); palettes are
//! shared as `Arc`s where the reference shares tuples.

use std::sync::Arc;

use crate::vdp::{HName, VdpState};

/// How many pixels the picture's output runs behind the H counter, by mode (Nuked-MD
/// shows a CRAM write committed at its H counter h from output pixel h - 24, and its
/// H40 slots are numbered three above the board's; so 21 in H40 and 24 in H32, which
/// puts H40's slot $012 at X = -3, Kabuto's figure). docs/worklog.md, "Where a
/// mid-line write shows".
pub fn output_delay(mode: HName) -> i64 {
    match mode {
        HName::H40 => 21,
        HName::H32 => 24,
    }
}

/// When each plane's tiles are fetched, in the board's pixel time, relative to the
/// start 16g of the 2-cell group g being fetched: (the group's name, its first cell's
/// pattern row, its second's), planes A and B (Nuked-MD's own VRAM accesses;
/// docs/worklog.md, "Where tiles are fetched").
pub fn fetch_at(mode: HName) -> [[i64; 3]; 2] {
    match mode {
        HName::H40 => [[0, 4, 6], [8, 12, 14]],
        HName::H32 => [[3, 7, 9], [11, 15, 17]],
    }
}

/// The VDP's measured output levels, 0-255, for the 15 steps of its DAC ramp
/// (Plutiedev `vdp-color-ramp`).
pub const RAMP: [u32; 15] = [
    0, 29, 52, 70, 87, 101, 116, 130, 144, 158, 172, 187, 206, 228, 255,
];
/// Normal (every other value), shadow (the first eight) and highlight (the last eight).
pub const LEVELS: [[u32; 8]; 3] = [
    [
        RAMP[0], RAMP[2], RAMP[4], RAMP[6], RAMP[8], RAMP[10], RAMP[12], RAMP[14],
    ],
    [
        RAMP[0], RAMP[1], RAMP[2], RAMP[3], RAMP[4], RAMP[5], RAMP[6], RAMP[7],
    ],
    [
        RAMP[7], RAMP[8], RAMP[9], RAMP[10], RAMP[11], RAMP[12], RAMP[13], RAMP[14],
    ],
];

/// A pixel of a rendered line: the CRAM index in bits 5-0, the shade in bits 7-6 (0
/// normal, 1 shadow, 2 highlight).
pub const SHADE_SHIFT: u32 = 6;

/// Sprites examined per frame, sprites per line, and sprite tiles per line, by display
/// width (Nemesis, forum t=541 start=30, checked on an MD1600; Kabuto, phases 1 and 3).
pub fn sprite_limits(width: usize) -> (usize, usize, i64) {
    if width == 256 {
        (64, 16, 32)
    } else {
        (80, 20, 40)
    }
}

/// A plane's size by the two bits of register 16: cells across, how far the tile row
/// is shifted to give the name table row's offset, and the vertical wrap mask (CMD
/// `genvdp.txt`, register $10; Genesis Plus GX's tables for the invalid size).
pub const PLANE_CELLS: [i64; 4] = [32, 64, 32, 128];
pub const PLANE_ROW_SHIFT: [i64; 4] = [6, 7, 0, 8];
pub const PLANE_ROW_MASK: [i64; 4] = [0x0FF, 0x1FF, 0x2FF, 0x3FF];

/// CRAM as RGB888: normal, shadowed and highlighted.
pub type Palette = [[u32; 64]; 3];

/// What phase 2 latched for one sprite: x, attribute, flip_x, cells_x, cells_y,
/// pattern, cell row, pattern line.
type Latched = (i64, u8, bool, i64, i64, i64, i64, i64);

/// The line setup (`_plane_setup`): scroll A and B, the window's first and last
/// columns, where the window bug starts, the screen row, interlace mode 2, the width.
#[derive(Clone, Copy, Debug, Default)]
struct Spans {
    scroll_a: i64,
    scroll_b: i64,
    window_first: i64,
    window_last: i64,
    window_bug: i64,
    screen_row: i64,
    im2: bool,
    width: usize,
}

/// One pattern row as eight pixels: the colour in bits 3-0, the tile's palette in bits
/// 5-4 and its priority in bit 6 -- a transparent pixel keeps the palette and priority
/// too (Kabuto, "S/H"; BlastEm's `render_map` keeps both the same way).
fn tile_row(vram: &[u8], address: usize, attribute: u8, flip: bool) -> [u8; 8] {
    let data = u32::from(vram[address]) << 24
        | u32::from(vram[address + 1]) << 16
        | u32::from(vram[address + 2]) << 8
        | u32::from(vram[address + 3]);
    let mut pixels = [0u8; 8];
    for (i, pixel) in pixels.iter_mut().enumerate() {
        *pixel = attribute | ((data >> (28 - 4 * i)) & 15) as u8;
    }
    if flip {
        pixels.reverse();
    }
    pixels
}

/// The picture of one VDP.
pub struct Renderer {
    /// The frame being drawn: `pixels[row][x]` is a CRAM index and a shade. 480 rows
    /// are kept so that double interlace fits.
    pub pixels: Vec<Vec<u8>>,
    /// The picture's size once a frame is done, and the width each row was drawn at.
    pub width: usize,
    pub widths: Vec<usize>,
    pub height: usize,
    pub rows: usize,
    pub done_width: usize,
    done_pixels: Vec<Vec<u8>>,
    done_widths: Vec<usize>,
    /// Whether the sprite read last had X != 0 (Kabuto, phase 3).
    pub last_x_nonzero: bool,
    /// Status bits 6 and 5, cleared when the status is read.
    pub overflow: bool,
    pub collision: bool,
    /// The palettes each row was drawn with, as `(first pixel, palette)` in order.
    palettes: Vec<Vec<(usize, Arc<Palette>)>>,
    done_palettes: Vec<Vec<(usize, Arc<Palette>)>>,
    palette: Option<Arc<Palette>>,
    /// Set by the VDP on a CRAM write: the next pixels drawn take a fresh palette.
    pub cram_dirty: bool,
    /// CRAM dots: `(pixel, RGB)` per row, from a CRAM write during the display.
    dots: Vec<Vec<(usize, u32)>>,
    done_dots: Vec<Vec<(usize, u32)>>,
    /// Rows drawn with register $00 bit 0 set: no display generation at all (CMD
    /// `genvdp.txt` 17).
    blanked: Vec<u8>,
    done_blanked: Vec<u8>,
    /// The line being drawn (`begin_line`).
    pub pending: bool,
    line_start: i64,
    drawn: i64,
    row: usize,
    line_width: usize,
    backdrop: u8,
    line_a: Vec<u8>,
    line_b: Vec<u8>,
    line_s: Vec<u8>,
    spans: Option<Spans>,
    fetched: [i64; 2],
    fine: [i64; 2],
    /// The sprite cache: the Y and size/link words of each of the 80 sprites, written
    /// through by VRAM writes that land in the sprite table (Kabuto, "General").
    pub sprite_cache: [u8; 80 * 4],
    /// What the line before scanned for the next line, as
    /// `((row, width, double interlace), latched sprites)`.
    scanned: Option<((i64, usize, bool), Vec<Latched>)>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Renderer {
            pixels: vec![vec![0; 320]; 480],
            width: 320,
            widths: vec![0; 480],
            height: 0,
            rows: 0,
            done_width: 320,
            done_pixels: vec![vec![0; 320]; 480],
            done_widths: vec![0; 480],
            last_x_nonzero: false,
            overflow: false,
            collision: false,
            palettes: vec![Vec::new(); 480],
            done_palettes: vec![Vec::new(); 480],
            palette: None,
            cram_dirty: true,
            dots: vec![Vec::new(); 480],
            done_dots: vec![Vec::new(); 480],
            blanked: vec![0; 480],
            done_blanked: vec![0; 480],
            pending: false,
            line_start: -1,
            drawn: 0,
            row: 0,
            line_width: 320,
            backdrop: 0,
            line_a: Vec::new(),
            line_b: Vec::new(),
            line_s: Vec::new(),
            spans: None,
            fetched: [0, 0],
            fine: [0, 0],
            sprite_cache: [0; 80 * 4],
            scanned: None,
        }
    }

    // -- the sprite cache -----------------------------------------------------------

    /// A VRAM byte write at `address` (17 bits): update the cache if it lands in the
    /// first four bytes of a sprite table entry ("this mechanism is sensitive to the
    /// entire 128 KB address range even in 64 KB mode", Kabuto). The table's base is
    /// register 5 shifted left 9, with bit 9 ignored in H40.
    pub fn sprite_cache_write(&mut self, vdp: &VdpState, address: u32, value: u8) {
        let mut base = (i64::from(vdp.registers[5]) << 9) & 0x1FFFF;
        if vdp.hmode.name == HName::H40 {
            base &= !0x200;
        }
        let offset = i64::from(address) - base;
        if (0..80 * 8).contains(&offset) && offset & 4 == 0 {
            self.sprite_cache[((offset >> 3) * 4 + (offset & 3)) as usize] = value;
        }
    }

    // -- the planes -------------------------------------------------------------------

    /// One scrolling plane, over the screen pixels `first`..`last` - 1 (`row` is the
    /// plane row in pixels before its vertical wrap, `size` register 16, `scroll` the
    /// plane's horizontal scroll, and `window_bug` the screen pixel where CMD's window
    /// bug starts, or -1).
    #[allow(clippy::too_many_arguments)]
    fn plane_columns(
        vram: &[u8],
        line: &mut [u8],
        base: i64,
        row: i64,
        size: u8,
        scroll: i64,
        im2: bool,
        window_bug: i64,
        first: i64,
        last: i64,
    ) {
        let tile_rows: i64 = if im2 { 16 } else { 8 };
        let (pattern_mask, pattern_bytes) = if im2 { (0x3FF, 64) } else { (0x7FF, 32) };
        let row_mask = PLANE_ROW_MASK[usize::from((size >> 4) & 3)];
        let row = row & if im2 { (row_mask << 1) | 1 } else { row_mask };
        let name_row =
            base + (((row / tile_rows) << PLANE_ROW_SHIFT[usize::from(size & 3)]) & 0x1FC0);
        let pattern_row = row & (tile_rows - 1);
        let column_mask = PLANE_CELLS[usize::from(size & 3)] - 1;
        let mut x = first - ((first - scroll) & 7);
        while x < last {
            let mut cell = ((x - scroll) >> 3) & column_mask;
            if window_bug >= 0 && (0..16).contains(&(x - window_bug)) {
                cell = (cell + 2) & column_mask;
            }
            let entry = ((name_row + cell * 2) & 0xFFFF) as usize;
            let name = i64::from(vram[entry]) << 8 | i64::from(vram[(entry + 1) & 0xFFFF]);
            let pattern_line = if name & 0x1000 != 0 {
                tile_rows - 1 - pattern_row
            } else {
                pattern_row
            };
            let address =
                (((name & pattern_mask) * pattern_bytes + pattern_line * 4) & 0xFFFF) as usize;
            let pixels = tile_row(
                vram,
                address,
                ((name >> 9) & 0x70) as u8,
                name & 0x0800 != 0,
            );
            if first <= x && x + 8 <= last {
                line[x as usize..x as usize + 8].copy_from_slice(&pixels);
            } else {
                for (i, &pixel) in pixels.iter().enumerate() {
                    let at = x + i as i64;
                    if first <= at && at < last {
                        line[at as usize] = pixel;
                    }
                }
            }
            x += 8;
        }
    }

    /// The window's cells `first`..`last`, unscrolled, clipped to the screen pixels
    /// `left`..`right` - 1. Its name table is 32 cells wide in H32 and 64 in H40, and
    /// register 3's base masks to 0xF800, or 0xF000 in H40 (CMD `genvdp.txt`).
    #[allow(clippy::too_many_arguments)]
    fn window_line(
        vdp: &VdpState,
        line: &mut [u8],
        width: usize,
        row: i64,
        first: i64,
        last: i64,
        im2: bool,
        left: i64,
        right: i64,
    ) {
        let vram = &vdp.vram;
        let columns: i64 = if width == 320 { 64 } else { 32 };
        let base = (i64::from(vdp.registers[3]) << 10) & if width == 320 { 0xF000 } else { 0xF800 };
        let tile_rows: i64 = if im2 { 16 } else { 8 };
        let (pattern_mask, pattern_bytes) = if im2 { (0x3FF, 64) } else { (0x7FF, 32) };
        let name_row = base + (row / tile_rows) * columns * 2;
        let pattern_row = row & (tile_rows - 1);
        for cell in first.max(left >> 3)..=last.min((right - 1) >> 3) {
            let entry = ((name_row + cell * 2) & 0xFFFF) as usize;
            let name = i64::from(vram[entry]) << 8 | i64::from(vram[(entry + 1) & 0xFFFF]);
            let pattern_line = if name & 0x1000 != 0 {
                tile_rows - 1 - pattern_row
            } else {
                pattern_row
            };
            let address =
                (((name & pattern_mask) * pattern_bytes + pattern_line * 4) & 0xFFFF) as usize;
            let x = (cell * 8) as usize;
            line[x..x + 8].copy_from_slice(&tile_row(
                vram,
                address,
                ((name >> 9) & 0x70) as u8,
                name & 0x0800 != 0,
            ));
        }
    }

    /// The first and last cell the window covers on line `v` (CMD `genvdp.txt`,
    /// registers $11 and $12); `(0, -1)` when it covers nothing.
    fn window_columns(vdp: &VdpState, cells: i64, v: i64) -> (i64, i64) {
        let registers = &vdp.registers;
        let vertical = i64::from(registers[18] & 0x1F);
        let line_cell = v >> 3;
        let covers = if registers[18] & 0x80 != 0 {
            line_cell >= vertical
        } else {
            line_cell < vertical
        };
        if covers {
            return (0, cells - 1);
        }
        let horizontal = i64::from(registers[17] & 0x1F) * 2;
        if registers[17] & 0x80 != 0 {
            return if horizontal < cells {
                (horizontal, cells - 1)
            } else {
                (0, -1)
            };
        }
        if horizontal != 0 {
            (0, horizontal.min(cells) - 1)
        } else {
            (0, -1)
        }
    }

    /// What both planes and the window need for the whole of line `v`, read at the
    /// line's start: the horizontal scroll entry and the window's columns are fetched
    /// once a line (Kabuto's "H" access is the first of the line).
    fn plane_setup(vdp: &VdpState, width: usize, v: i64, screen_row: i64, im2: bool) -> Spans {
        let registers = &vdp.registers;
        let vram = &vdp.vram;
        let cells = (width >> 3) as i64;
        // Horizontal scroll: full screen, the first eight entries, every cell, or every
        // line (CMD `genvdp.txt`, register $0B bits 1-0).
        let mask = [0, 7, 0xFFF8, 0xFFFF][usize::from(registers[11] & 3)];
        // Register 13 bits 5-0 are A15-A10 of the scroll table.
        let table = (i64::from(registers[13]) << 10) & 0xFC00;
        // The scroll table is read with the V counter's line, not the doubled one, in
        // interlace mode 2 (Genesis Plus GX `render_bg_m5_im2`).
        let entry = ((table + (v & mask) * 4) & 0xFFFF) as usize;
        let word =
            |at: usize| i64::from(vram[at & 0xFFFF]) << 8 | i64::from(vram[(at + 1) & 0xFFFF]);
        let scroll_a = word(entry) & 0x3FF;
        let scroll_b = word(entry + 2) & 0x3FF;
        let (window_first, window_last) = Renderer::window_columns(vdp, cells, v);
        let mut window_bug = -1;
        if window_first == 0 && 0 <= window_last && window_last < cells - 1 && scroll_a & 0x0F != 0
        {
            // CMD's window bug: the partial column the fine scroll puts before plane A's
            // first whole 2-cell column fetches its names from two cells on (his capture
            // photos `cv-win-00.gif` and `cv-win-01.gif`).
            window_bug = (window_last + 1) * 8 - 16 + (scroll_a & 0x0F);
        }
        Spans {
            scroll_a,
            scroll_b,
            window_first,
            window_last,
            window_bug,
            screen_row,
            im2,
            width,
        }
    }

    /// Fetch planes A (with the window over it) and B over the screen pixels
    /// `first`..`last` - 1, reading VRAM, VSRAM and the registers as they stand.
    fn plane_span(&mut self, vdp: &VdpState, first: i64, last: i64, planes: &[usize]) {
        let spans = self.spans.expect("a line under way");
        let registers = &vdp.registers;
        let size = registers[16];
        let two_cell = registers[11] & 4 != 0;
        let words = vdp.vsram_words;
        // Register 2 bits 5-3 are plane A's A15-A13 and register 4 bits 2-0 plane B's.
        let bases = [
            (i64::from(registers[2]) << 10) & 0xE000,
            (i64::from(registers[4]) << 13) & 0xE000,
        ];
        let scrolls = [spans.scroll_a, spans.scroll_b];
        for plane in [0usize, 1] {
            if !planes.contains(&plane) {
                continue;
            }
            let line = if plane == 0 {
                &mut self.line_a
            } else {
                &mut self.line_b
            };
            if two_cell {
                // Two-cell column vertical scroll: each 16-pixel column has its own VSRAM
                // entry (CMD `genvdp.txt`, register $0B bit 2).
                let mut x = first & !15;
                while x < last {
                    let index = ((x >> 4) * 2) as usize + plane;
                    let vscroll = if index < words {
                        i64::from(vdp.column_scroll(index))
                    } else {
                        0
                    };
                    Renderer::plane_columns(
                        &vdp.vram,
                        line,
                        bases[plane],
                        spans.screen_row + vscroll,
                        size,
                        scrolls[plane],
                        spans.im2,
                        spans.window_bug,
                        x.max(first),
                        (x + 16).min(last),
                    );
                    x += 16;
                }
            } else {
                // Full-screen: the value latched at the end of the line before.
                Renderer::plane_columns(
                    &vdp.vram,
                    line,
                    bases[plane],
                    spans.screen_row + i64::from(vdp.vscroll_latch[plane]),
                    size,
                    scrolls[plane],
                    spans.im2,
                    spans.window_bug,
                    first,
                    last,
                );
            }
        }
        if spans.window_last >= spans.window_first && planes.contains(&0) {
            // "Plane A is not shown in any column where plane W is shown" (CMD).
            Renderer::window_line(
                vdp,
                &mut self.line_a,
                spans.width,
                spans.screen_row,
                spans.window_first,
                spans.window_last,
                spans.im2,
                first,
                last,
            );
        }
    }

    // -- sprites ----------------------------------------------------------------------

    /// Phases 1 and 2 for display row `screen_row`, which the hardware runs a line ahead
    /// of it (Kabuto). Phase 1 walks the cached linked list and keeps the first 20 (16)
    /// sprites whose rows cover the line; phase 2 reads each one's X and pattern from
    /// the table at register 5 as it stands, and Y and size from the cache.
    fn sprite_scan(
        &mut self,
        vdp: &VdpState,
        width: usize,
        screen_row: i64,
        im2: bool,
    ) -> Vec<Latched> {
        let vram = &vdp.vram;
        let cache = &self.sprite_cache;
        let (max_sprites, max_line, _) = sprite_limits(width);
        let tile_rows: i64 = if im2 { 16 } else { 8 };
        let start_v: i64 = if im2 { 0x100 } else { 0x80 };
        let mask_v: i64 = if im2 { 0x3FF } else { 0x1FF };
        let cached_y = |entry: usize| {
            ((i64::from(cache[entry]) << 8 | i64::from(cache[entry + 1])) & mask_v) - start_v
        };

        let mut visible: Vec<usize> = Vec::new();
        let mut overflow = false;
        let mut index = 0usize;
        for _ in 0..max_sprites {
            let entry = index * 4;
            let y = cached_y(entry);
            if y <= screen_row && screen_row < y + (i64::from(cache[entry + 2] & 3) + 1) * tile_rows
            {
                if visible.len() < max_line {
                    visible.push(index);
                } else {
                    overflow = true; // more sprites on this line than there are slots
                }
            }
            let link = usize::from(cache[entry + 3] & 0x7F);
            if link == 0 || link >= max_sprites {
                // "a link attribute of 0 makes it stop scanning the list further"
                // (Kabuto); an entry past the per-frame limit ends the list (Nemesis).
                break;
            }
            index = link;
        }

        let mut base = (i64::from(vdp.registers[5]) << 9) & 0xFFFF;
        if width == 320 {
            base &= !0x200;
        }
        let pattern_mask = if im2 { 0x3FF } else { 0x7FF };
        let mut latched = Vec::with_capacity(visible.len());
        for sprite in visible {
            let entry = ((base + sprite as i64 * 8) & 0xFFFF) as usize;
            let cached = sprite * 4;
            let y = cached_y(cached);
            let size = cache[cached + 2];
            let cells_x = i64::from((size >> 2) & 3) + 1;
            let cells_y = i64::from(size & 3) + 1;
            let at = |i: usize| i64::from(vram[(entry + i) & 0xFFFF]);
            let name = at(4) << 8 | at(5);
            let x = ((at(6) << 8 | at(7)) & 0x1FF) - 0x80;
            // "The VDP just considers the lowermost 5 bits of the difference of sprite
            // Y coordinate and raster line" (Kabuto, phase 2), doubled in interlace 2.
            let mut row = (screen_row - y) & if im2 { 0x3F } else { 0x1F };
            if name & 0x1000 != 0 {
                row = cells_y * tile_rows - 1 - row;
            }
            latched.push((
                x,
                ((name >> 9) & 0x70) as u8,
                name & 0x0800 != 0,
                cells_x,
                cells_y,
                name & pattern_mask,
                row / tile_rows,
                row & (tile_rows - 1),
            ));
        }
        if overflow {
            self.overflow = true;
        }
        latched
    }

    /// Phase 3: fetch the latched sprites' tiles and render them into the line buffer,
    /// 40 (32) tiles at most, earlier sprites winning every pixel, with the X = 0 rule
    /// masking the rest of the line.
    fn sprite_draw(&mut self, vram: &[u8], width: usize, latched: &[Latched], im2: bool) {
        let pattern_bytes: i64 = if im2 { 64 } else { 32 };
        let mut tiles_left = sprite_limits(width).2;
        let mut rendering = true;
        let mut last_x_nonzero = self.last_x_nonzero;
        let line = &mut self.line_s;
        let mut collision = false;
        for &(x, attribute, flip_x, cells_x, cells_y, pattern, cell_row, pattern_line) in latched {
            if x == -0x80 {
                // "Rendering of further sprite pixels is disabled whenever a sprite is
                // read from a cache slot with X=0 when the previous sprite read ... had
                // X != 0" (Kabuto, phase 3); X = 0 is screen -128 here.
                if last_x_nonzero {
                    rendering = false;
                }
                last_x_nonzero = false;
            } else {
                last_x_nonzero = true;
            }
            let mut out_of_tiles = false;
            for cell in 0..cells_x {
                if tiles_left == 0 {
                    out_of_tiles = true; // the sprite is cut off partway: the dots ran out
                    break;
                }
                tiles_left -= 1;
                if !rendering {
                    continue;
                }
                let column = if flip_x { cells_x - 1 - cell } else { cell };
                let tile = pattern + column * cells_y + cell_row;
                let address = ((tile * pattern_bytes + pattern_line * 4) & 0xFFFF) as usize;
                let left = x + cell * 8;
                if left <= -8 || left >= width as i64 {
                    continue;
                }
                let pixels = tile_row(vram, address, attribute, flip_x);
                for (i, &pixel) in pixels.iter().enumerate() {
                    // A transparent pixel still carries its sprite's palette and
                    // priority, and any pixel whose colour bits are 0 is overwritten
                    // (Kabuto, phase 3).
                    let at = left + i as i64;
                    if 0 <= at && at < width as i64 {
                        let at = at as usize;
                        if line[at] & 15 == 0 {
                            line[at] = pixel;
                        } else if pixel & 15 != 0 {
                            collision = true;
                        }
                    }
                }
            }
            if out_of_tiles {
                break;
            }
        }
        if collision {
            self.collision = true;
        }
        if tiles_left != 0 {
            // "When there are less than 40 sprite tiles on a raster line then further
            // dummy sprites act as if they had X=0" (Kabuto).
            last_x_nonzero = false;
        } else {
            self.overflow = true;
        }
        self.last_x_nonzero = last_x_nonzero;
    }

    // -- one line ---------------------------------------------------------------------

    /// Start display line `v`, the V counter's value for it: finish the line before
    /// it, then read everything this one needs for its whole length.
    pub fn begin_line(&mut self, vdp: &VdpState, v: i64) {
        self.finish_line(vdp);
        let width = if vdp.hmode.name == HName::H40 {
            320
        } else {
            256
        };
        let im2 = vdp.interlace_double;
        let odd = i64::from(vdp.odd);
        let row = if im2 { (v << 1) | odd } else { v };
        // The sprites of the line after this one are scanned now (phases 1 and 2).
        let scanned = self.scanned.take();
        let after = (v + 1) & 0x1FF;
        let after_row = if im2 { (after << 1) | odd } else { after };
        if row >= 480 {
            // The line before the first (V = 511) is not shown, but its sprite scan is
            // the first line's (Kabuto, "Topmost and bottommost raster line").
            if vdp.display_enabled() && after_row < 480 {
                self.scan_ahead(vdp, after_row, im2);
            }
            return;
        }
        let row_index = row as usize;
        if self.cram_dirty || self.palette.is_none() {
            self.palette = Some(Arc::new(make_palette(&vdp.cram)));
            self.cram_dirty = false;
        }
        self.palettes[row_index] = vec![(0, self.palette.clone().expect("made above"))];
        if !self.dots[row_index].is_empty() {
            self.dots[row_index] = Vec::new();
        }
        self.width = width;
        if row_index + 1 > self.rows {
            self.rows = row_index + 1;
        }
        let backdrop = vdp.registers[7] & 0x3F;
        self.widths[row_index] = width;
        self.blanked[row_index] = vdp.registers[0] & 0x01;
        let out = &mut self.pixels[row_index];
        if width < 320 {
            out[width..320].fill(backdrop);
        }
        if !vdp.display_enabled() {
            // The backdrop colour; what the planes show on hardware is not modelled.
            out[..width].fill(backdrop);
            self.last_x_nonzero = false; // no sprites are fetched with the display off
            return;
        }
        self.line_a = vec![0; width];
        self.line_b = vec![0; width];
        self.line_s = vec![0; width];
        // The sprites of a line are fetched and drawn into the line buffer during the
        // horizontal blank before it (Kabuto, phase 3): done once, here, from what the
        // line before scanned, or scanned now if nothing did.
        match scanned {
            Some((key, latched)) if key == (row, width, im2) => {
                self.sprite_draw(&vdp.vram, width, &latched, im2);
            }
            _ => {
                let latched = self.sprite_scan(vdp, width, row, im2);
                self.sprite_draw(&vdp.vram, width, &latched, im2);
            }
        }
        if after_row < 480 {
            self.scan_ahead(vdp, after_row, im2);
        }
        let spans = Renderer::plane_setup(vdp, width, v, row, im2);
        self.spans = Some(spans);
        // How far each plane has been fetched, and each plane's fine scroll.
        self.fetched = [0, 0];
        self.fine = [spans.scroll_a & 15, spans.scroll_b & 15];
        self.row = row_index;
        self.line_width = width;
        self.backdrop = backdrop;
        self.drawn = 0;
        self.line_start = vdp.line_start;
        self.pending = true;
    }

    /// Run phases 1 and 2 for display row `row`, the next line's, and keep them.
    fn scan_ahead(&mut self, vdp: &VdpState, row: i64, im2: bool) {
        let width = if vdp.hmode.name == HName::H40 {
            320
        } else {
            256
        };
        let latched = self.sprite_scan(vdp, width, row, im2);
        self.scanned = Some(((row, width, im2), latched));
    }

    /// Bring the line under way up to `offset` master clocks into it, before something
    /// it is drawn from changes: every tile whose fetch time has come is fetched first
    /// (with `names`, every tile whose *name* has been fetched), then the output is
    /// mixed up to the pixel the H counter names less the output delay. `limit`: a
    /// pixel not to fetch past yet (a 2-cell column still to be latched).
    pub fn catch_up(&mut self, vdp: &VdpState, offset: i64, names: bool, limit: Option<i64>) {
        if !self.pending || vdp.line_start != self.line_start {
            return;
        }
        let mode = vdp.hmode;
        let t = mode.position(offset) - mode.display_at as i64;
        self.fetch_until(vdp, t, names, limit);
        self.draw_to(vdp, t - output_delay(mode.name));
    }

    /// Whether the line under way shows the window plane anywhere.
    pub fn shows_window(&self) -> bool {
        self.pending && self.spans.is_some_and(|s| s.window_last >= s.window_first)
    }

    /// Fetch `plane` over the screen pixels up to `x`, from the state as it stands.
    pub fn fetch_to(&mut self, vdp: &VdpState, plane: usize, x: i64) {
        let x = x.min(self.line_width as i64);
        if x > self.fetched[plane] {
            self.plane_span(vdp, self.fetched[plane], x, &[plane]);
            self.fetched[plane] = x;
        }
    }

    /// Fetch every cell of both planes whose fetch time is at or before `t`.
    fn fetch_until(&mut self, vdp: &VdpState, t: i64, names: bool, limit: Option<i64>) {
        let offsets = fetch_at(vdp.hmode.name);
        for plane in [0usize, 1] {
            let [name_at, even_at, odd_at] = offsets[plane];
            let fine = self.fine[plane];
            // Cells in fetch order, from the partial column (group -1) on: cell c starts
            // at screen pixel 8c + fine and belongs to group c >> 1.
            let mut cell = (self.fetched[plane] - fine).div_euclid(8);
            loop {
                let group = cell >> 1;
                let at = 16 * group
                    + if names {
                        name_at
                    } else if cell & 1 != 0 {
                        odd_at
                    } else {
                        even_at
                    };
                let start = 8 * cell + fine;
                if at > t || start >= self.line_width as i64 {
                    break;
                }
                cell += 1;
            }
            let mut x = (8 * cell + fine).max(0);
            if let Some(limit) = limit {
                x = x.min(limit);
            }
            self.fetch_to(vdp, plane, x);
        }
    }

    /// Draw whatever is left of the line under way.
    pub fn finish_line(&mut self, vdp: &VdpState) {
        if self.pending {
            self.draw_to(vdp, self.line_width as i64);
            self.pending = false;
        }
    }

    /// A CRAM write landing at an access slot during the active display shows the
    /// colour it wrote, for the one pixel of that slot (BlastEm `write_cram`, `vdp.c`
    /// lines 443-451, a detector). The caller has already drawn the line up to it.
    pub fn cram_dot(&mut self, colour: u16) {
        let x = self.drawn;
        if self.pending && 0 <= x && x < self.line_width as i64 {
            let levels = LEVELS[0];
            let colour = usize::from(colour);
            self.dots[self.row].push((
                x as usize,
                levels[(colour >> 1) & 7] << 16
                    | levels[(colour >> 5) & 7] << 8
                    | levels[(colour >> 9) & 7],
            ));
        }
    }

    /// Draw the line's pixels from where it had got to up to `x`.
    fn draw_to(&mut self, vdp: &VdpState, x: i64) {
        let first = self.drawn;
        let last = x.min(self.line_width as i64);
        if last <= first {
            return;
        }
        if !vdp.display_enabled() {
            // The display was turned off part way along the line: the rest is the
            // backdrop (Kabuto, "Effects of 'disable display'").
            let backdrop = vdp.registers[7] & 0x3F;
            self.pixels[self.row][first as usize..last as usize].fill(backdrop);
            self.drawn = last;
            return;
        }
        if self.cram_dirty {
            // CRAM changed part way along the line: the rest of it is drawn with the new
            // colours, and the pixels already drawn keep the ones they had.
            let palette = Arc::new(make_palette(&vdp.cram));
            self.palette = Some(palette.clone());
            self.cram_dirty = false;
            self.palettes[self.row].push((first as usize, palette));
        }
        self.fetch_to(vdp, 0, last);
        self.fetch_to(vdp, 1, last);
        // The backdrop index is read as it stands: a mid-line write to register 7
        // changes the rest of the line (Nuked-MD; docs/worklog.md).
        self.mix(vdp, first as usize, last as usize, vdp.registers[7] & 0x3F);
        self.drawn = last;
    }

    // -- the mixer ----------------------------------------------------------------------

    /// Priority (CMD `genvdp.txt` 14: backdrop < B low < A low < sprite low < B high <
    /// A high < sprite high) and shadow/highlight (Kabuto, "S/H": the three steps of his
    /// "assumed operation mode", in order), pixel by pixel. The conditions keep the
    /// reference's shape.
    #[allow(clippy::nonminimal_bool, clippy::needless_range_loop)]
    fn mix(&mut self, vdp: &VdpState, first: usize, last: usize, backdrop: u8) {
        let shadow_mode = vdp.registers[12] & 8 != 0;
        let debug = vdp.debug_register;
        let force = ((debug >> 7) & 3) as usize;
        let blank = debug & 0x40 != 0;
        let out = &mut self.pixels[self.row];
        for x in first..last {
            let av = self.line_a[x];
            let bv = self.line_b[x];
            let sv = self.line_s[x];
            let a_high = av & 0x40 != 0;
            let b_high = bv & 0x40 != 0;
            let mut s_visible = sv & 15 != 0
                && (sv & 0x40 != 0 || (!(av & 15 != 0 && a_high) && !(bv & 15 != 0 && b_high)));
            let mut shade = 0u8;
            if shadow_mode {
                let colour = sv & 0x3F;
                // 1: a visible sprite pixel of colour 62 or 63 is not drawn and turns
                // the shade on instead.
                if s_visible && colour >= 62 {
                    s_visible = false;
                    shade = if colour == 63 { 1 } else { 2 };
                }
                // 2: both planes low priority, the sprite plane inactive or low
                // priority: shadow, and no highlight.
                if !a_high && !b_high && !(s_visible && sv & 0x40 != 0) {
                    shade = 1;
                }
                // 3: a sprite pixel whose lowest four bits are 14 cancels the shadow.
                if shade == 1 && sv & 0x0F == 14 {
                    shade = 0;
                }
            }
            let mut value: i16 = if s_visible {
                i16::from(sv & 0x3F)
            } else if av & 15 != 0 && a_high {
                i16::from(av & 0x3F)
            } else if bv & 15 != 0 && b_high {
                i16::from(bv & 0x3F)
            } else if av & 15 != 0 {
                i16::from(av & 0x3F)
            } else if bv & 15 != 0 {
                i16::from(bv & 0x3F)
            } else {
                -1 // no plane is showing: the backdrop
            };
            if force != 0 || blank {
                value = i16::from(debug_pixel(value, av, bv, sv, force, blank, backdrop));
            } else if value < 0 {
                value = i16::from(backdrop);
            }
            out[x] = value as u8 | (shade << SHADE_SHIFT);
        }
    }

    // -- frames -------------------------------------------------------------------------

    /// A new frame begins: the rows drawn so far become the finished picture, handed
    /// over by swapping the row arrays (so a step that carries the VDP past the frame
    /// boundary cannot overwrite the picture a caller is about to read).
    pub fn start_frame(&mut self, vdp: &VdpState) {
        self.finish_line(vdp);
        if self.rows != 0 {
            self.height = self.rows;
            let widest = self.widths[..self.rows]
                .iter()
                .copied()
                .max()
                .unwrap_or(self.width);
            self.done_width = if widest != 0 { widest } else { self.width };
            std::mem::swap(&mut self.pixels, &mut self.done_pixels);
            std::mem::swap(&mut self.widths, &mut self.done_widths);
            std::mem::swap(&mut self.blanked, &mut self.done_blanked);
            std::mem::swap(&mut self.palettes, &mut self.done_palettes);
            std::mem::swap(&mut self.dots, &mut self.done_dots);
        }
        self.rows = 0;
    }

    /// The finished frame's size and which set of arrays holds it (`true`: the
    /// finished ones; before any frame has finished, the ones being drawn into).
    fn finished(&self, interlace_double: bool) -> (usize, usize, bool) {
        let (width, mut height, done) = if self.height != 0 {
            (self.done_width, self.height, true)
        } else {
            // Power-on: nothing has finished yet, so show what has been drawn.
            let height = self.rows;
            let widest = self.widths[..height]
                .iter()
                .copied()
                .max()
                .unwrap_or(self.width);
            (if widest != 0 { widest } else { self.width }, height, false)
        };
        if interlace_double {
            height = (height + 1) & !1; // both fields make up the picture
        }
        (width, height, done)
    }

    /// The picture: its width, its height, and its rows of shaded CRAM indices.
    pub fn frame(&self, vdp: &VdpState) -> (usize, usize, Vec<&[u8]>) {
        let (width, height, done) = self.finished(vdp.interlace_double);
        let pixels = if done {
            &self.done_pixels
        } else {
            &self.pixels
        };
        (
            width,
            height,
            pixels[..height].iter().map(|r| r.as_slice()).collect(),
        )
    }

    /// The picture as rows of RGB888, each pixel through the palette it was drawn with;
    /// CRAM dots are laid over the result.
    pub fn frame_rgb(&self, vdp: &VdpState) -> Vec<Vec<u32>> {
        let (width, height, done) = self.finished(vdp.interlace_double);
        let (pixels, blanked, palettes_of, dots_of) = if done {
            (
                &self.done_pixels,
                &self.done_blanked,
                &self.done_palettes,
                &self.done_dots,
            )
        } else {
            (&self.pixels, &self.blanked, &self.palettes, &self.dots)
        };
        let default = vec![(
            0usize,
            self.palette
                .clone()
                .unwrap_or_else(|| Arc::new(make_palette(&vdp.cram))),
        )];
        let mut out = Vec::with_capacity(height);
        for index in 0..height {
            if blanked[index] != 0 {
                // Register $00 bit 0: no display generation at all (CMD `genvdp.txt` 17).
                out.push(vec![0; width]);
                continue;
            }
            let row = &pixels[index];
            let palettes = if palettes_of[index].is_empty() {
                &default
            } else {
                &palettes_of[index]
            };
            let mut line = vec![0u32; width];
            for (span, (start, palette)) in palettes.iter().enumerate() {
                let stop = palettes.get(span + 1).map_or(width, |next| next.0);
                for x in *start..stop.min(width) {
                    let value = row[x];
                    line[x] = palette[usize::from(value >> SHADE_SHIFT)][usize::from(value & 0x3F)];
                }
            }
            for &(x, colour) in &dots_of[index] {
                if x < width {
                    line[x] = colour;
                }
            }
            out.push(line);
        }
        out
    }
}

/// The debug register's plane forcing (Kabuto, "The mighty debug register"): bit 6
/// turns every plane off, bits 8-7 force one plane permanently active (01 sprites, 10
/// A, 11 B), and where another plane also shows the two are ANDed, his mixer
/// `(A|!a)&(B|!b)&(S|!s)&(G|!g)` with one plane's flag held at 1.
fn debug_pixel(value: i16, av: u8, bv: u8, sv: u8, force: usize, blank: bool, backdrop: u8) -> u8 {
    let forced = [0, sv, av, bv][force] & 0x3F;
    if blank {
        return if force != 0 { forced } else { backdrop };
    }
    if force == 0 {
        return if value < 0 { backdrop } else { value as u8 };
    }
    if value < 0 {
        return forced; // the backdrop was showing: the forced plane shows instead
    }
    value as u8 & forced
}

/// CRAM as RGB888: normal, shadowed and highlighted.
pub fn make_palette(cram: &[u16; 64]) -> Palette {
    let mut palette = [[0u32; 64]; 3];
    for (shade, levels) in LEVELS.iter().enumerate() {
        for (index, &colour) in cram.iter().enumerate() {
            let colour = usize::from(colour);
            palette[shade][index] = levels[(colour >> 1) & 7] << 16
                | levels[(colour >> 5) & 7] << 8
                | levels[(colour >> 9) & 7];
        }
    }
    palette
}
