//! Where icons sit on a desktop, as arithmetic.
//!
//! Deliberately free of GTK types: the grid is the part that has to be right,
//! and keeping it pure means it can be tested without a display.

use std::collections::BTreeSet;

use crate::ui::views::icon::TILE_EXTRA_WIDTH;

/// Gap between a tile's icon and its label.
const ICON_LABEL_SPACING: i32 = 6;
/// A label wraps to at most two lines, like the icon view's.
const LABEL_LINES: i32 = 2;
const LABEL_LINE_HEIGHT: i32 = 17;
/// Padding inside a tile, per edge.
const TILE_PADDING: i32 = 6;
/// Gap between the outermost tiles and the edge of the usable area.
const GRID_MARGIN: i32 = 12;

/// A grid slot, counted from the top-left of the usable area.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Cell {
    // Ordered col-then-row by the derive, which is exactly the fill order:
    // `BTreeSet<Cell>` iterates in the order cells get handed out.
    pub col: i32,
    pub row: i32,
}

impl Cell {
    pub fn new(col: i32, row: i32) -> Self {
        Self { col, row }
    }
}

/// The grid a given viewport and icon size imply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridMetrics {
    pub tile_width: i32,
    pub tile_height: i32,
    pub cell_width: i32,
    pub cell_height: i32,
    pub margin: i32,
    pub columns: i32,
    pub rows: i32,
    pub viewport: (i32, i32),
}

impl GridMetrics {
    pub fn new(viewport: (i32, i32), icon_size: i32, spacing: i32) -> Self {
        let icon_size = icon_size.max(1);
        let spacing = spacing.max(0);

        let tile_width = icon_size + TILE_EXTRA_WIDTH;
        let tile_height =
            icon_size + ICON_LABEL_SPACING + LABEL_LINES * LABEL_LINE_HEIGHT + TILE_PADDING * 2;
        let cell_width = tile_width + spacing;
        let cell_height = tile_height + spacing;

        let usable_width = viewport.0 - GRID_MARGIN * 2;
        let usable_height = viewport.1 - GRID_MARGIN * 2;

        Self {
            tile_width,
            tile_height,
            cell_width,
            cell_height,
            margin: GRID_MARGIN,
            // At least one column even on a viewport too small to hold a tile:
            // a zero-column grid has no valid cell to put anything in, and the
            // compositor can hand us a 1x1 surface before it settles.
            columns: (usable_width / cell_width).max(1),
            rows: (usable_height / cell_height).max(1),
            viewport,
        }
    }

    /// Top-left pixel of `cell`'s tile.
    pub fn cell_origin(&self, cell: Cell) -> (i32, i32) {
        (
            self.margin + cell.col * self.cell_width,
            self.margin + cell.row * self.cell_height,
        )
    }

    /// The cell a tile dropped at `(x, y)` snaps to.
    ///
    /// `(x, y)` is the tile's top-left, not the cursor: the caller subtracts
    /// wherever inside the tile the drag was grabbed, so a tile picked up by
    /// its bottom-right corner does not jump.
    pub fn cell_for_point(&self, x: i32, y: i32) -> Cell {
        let col = div_round(x - self.margin, self.cell_width);
        let row = div_round(y - self.margin, self.cell_height);

        Cell::new(col.clamp(0, self.columns - 1), row.clamp(0, self.rows - 1))
    }

    pub fn contains(&self, cell: Cell) -> bool {
        cell.col >= 0 && cell.col < self.columns && cell.row >= 0 && cell.row < self.rows
    }

    /// Pulls `cell` back inside the grid. Used when a stored position was made
    /// on a larger output than the one now showing it.
    pub fn clamp_cell(&self, cell: Cell) -> Cell {
        Cell::new(
            cell.col.clamp(0, self.columns - 1),
            cell.row.clamp(0, self.rows - 1),
        )
    }

    /// Keeps a freely-placed tile fully on screen.
    pub fn clamp_point(&self, x: i32, y: i32) -> (i32, i32) {
        // `max` after `min` so a viewport narrower than one tile clamps to the
        // margin rather than to a negative bound.
        let max_x = (self.viewport.0 - self.tile_width - self.margin).max(self.margin);
        let max_y = (self.viewport.1 - self.tile_height - self.margin).max(self.margin);

        (x.clamp(self.margin, max_x), y.clamp(self.margin, max_y))
    }
}

/// Rounds to nearest on a half-cell boundary, for negatives too.
///
/// Integer division truncates toward zero, so `-3 / 8` is `0` where the point
/// is a third of a cell off the left edge and should round to `0` anyway — but
/// `-12 / 8` is `-1` when it should be `-2`. Doing the arithmetic in floats
/// sidesteps the asymmetry entirely.
fn div_round(numerator: i32, denominator: i32) -> i32 {
    (f64::from(numerator) / f64::from(denominator.max(1))).round() as i32
}

/// The next `count` unoccupied cells, in the order a desktop fills: down the
/// first column, then down the second.
///
/// Overflows past the last column rather than returning fewer cells than asked
/// for. A caller that has more files than slots still needs a position for
/// every one of them; `GridMetrics::clamp_cell` stacks the overflow on the last
/// column, which is visible and recoverable. Returning short would leave files
/// with no position at all, which is not.
pub fn first_free_cells(
    metrics: GridMetrics,
    occupied: &BTreeSet<Cell>,
    count: usize,
) -> Vec<Cell> {
    let mut free = Vec::with_capacity(count);
    if count == 0 {
        return free;
    }

    let mut taken = occupied.clone();
    let mut col = 0;
    while free.len() < count {
        for row in 0..metrics.rows {
            let cell = Cell::new(col, row);
            if taken.insert(cell) {
                free.push(cell);
                if free.len() == count {
                    return free;
                }
            }
        }

        col += 1;
        if col == metrics.columns {
            tracing::warn!(
                columns = metrics.columns,
                rows = metrics.rows,
                wanted = count,
                "desktop grid is full; further icons will stack on the last column"
            );
        }
    }

    free
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(width: i32, height: i32, icon_size: i32) -> GridMetrics {
        GridMetrics::new((width, height), icon_size, 12)
    }

    #[test]
    fn a_grid_fits_whole_tiles_into_the_usable_area() {
        let grid = metrics(1920, 1080, 72);

        assert_eq!(grid.tile_width, 72 + TILE_EXTRA_WIDTH);
        assert!(grid.columns >= 1 && grid.rows >= 1);
        // The last column and row must fit entirely inside the viewport.
        let last = Cell::new(grid.columns - 1, grid.rows - 1);
        let (x, y) = grid.cell_origin(last);
        assert!(x + grid.tile_width <= 1920);
        assert!(y + grid.tile_height <= 1080);
    }

    #[test]
    fn a_bigger_icon_size_yields_fewer_columns() {
        let small = metrics(1920, 1080, 48);
        let large = metrics(1920, 1080, 128);

        assert!(small.columns > large.columns);
        assert!(small.rows > large.rows);
    }

    #[test]
    fn a_wider_output_yields_more_columns_at_the_same_row_count() {
        let narrow = metrics(1920, 1440, 72);
        let wide = metrics(3440, 1440, 72);

        assert!(wide.columns > narrow.columns);
        assert_eq!(wide.rows, narrow.rows);
    }

    /// A viewport smaller than a single tile still has to offer a slot — the
    /// compositor can configure a tiny surface before it settles, and a
    /// zero-column grid has nowhere to put anything.
    #[test]
    fn a_viewport_too_small_for_a_tile_still_has_one_cell() {
        let grid = metrics(10, 10, 128);

        assert_eq!(grid.columns, 1);
        assert_eq!(grid.rows, 1);
        assert!(grid.contains(Cell::new(0, 0)));
    }

    #[test]
    fn a_cell_origin_snaps_back_to_its_own_cell() {
        let grid = metrics(1920, 1080, 72);

        for col in 0..grid.columns {
            for row in 0..grid.rows {
                let cell = Cell::new(col, row);
                let (x, y) = grid.cell_origin(cell);
                assert_eq!(grid.cell_for_point(x, y), cell);
            }
        }
    }

    /// Just past the halfway point between two cells belongs to the second.
    #[test]
    fn a_point_past_the_half_cell_boundary_rounds_to_the_next_cell() {
        let grid = metrics(1920, 1080, 72);
        let (origin_x, origin_y) = grid.cell_origin(Cell::new(0, 0));

        let just_under = grid.cell_for_point(
            origin_x + grid.cell_width / 2 - 1,
            origin_y + grid.cell_height / 2 - 1,
        );
        assert_eq!(just_under, Cell::new(0, 0));

        let just_over = grid.cell_for_point(
            origin_x + grid.cell_width / 2 + 1,
            origin_y + grid.cell_height / 2 + 1,
        );
        assert_eq!(just_over, Cell::new(1, 1));
    }

    #[test]
    fn a_point_off_the_edge_clamps_into_the_grid() {
        let grid = metrics(1920, 1080, 72);

        assert_eq!(grid.cell_for_point(-9999, -9999), Cell::new(0, 0));
        assert_eq!(
            grid.cell_for_point(999_999, 999_999),
            Cell::new(grid.columns - 1, grid.rows - 1)
        );
    }

    #[test]
    fn a_free_placement_keeps_the_whole_tile_on_screen() {
        let grid = metrics(1920, 1080, 72);

        for (x, y) in [
            (-500, -500),
            (999_999, -500),
            (-500, 999_999),
            (999_999, 999_999),
        ] {
            let (cx, cy) = grid.clamp_point(x, y);
            assert!(cx >= grid.margin, "left edge: {cx}");
            assert!(cy >= grid.margin, "top edge: {cy}");
            assert!(cx + grid.tile_width <= 1920, "right edge: {cx}");
            assert!(cy + grid.tile_height <= 1080, "bottom edge: {cy}");
        }
    }

    #[test]
    fn a_point_already_inside_is_left_alone() {
        let grid = metrics(1920, 1080, 72);

        assert_eq!(grid.clamp_point(400, 300), (400, 300));
    }

    #[test]
    fn cells_are_handed_out_down_each_column_in_turn() {
        let grid = metrics(1920, 1080, 72);
        let free = first_free_cells(grid, &BTreeSet::new(), 3);

        assert_eq!(
            free,
            vec![Cell::new(0, 0), Cell::new(0, 1), Cell::new(0, 2)]
        );
    }

    #[test]
    fn a_full_column_carries_on_into_the_next() {
        let grid = metrics(1920, 1080, 72);
        let free = first_free_cells(grid, &BTreeSet::new(), grid.rows as usize + 1);

        assert_eq!(free[grid.rows as usize - 1], Cell::new(0, grid.rows - 1));
        assert_eq!(free[grid.rows as usize], Cell::new(1, 0));
    }

    #[test]
    fn occupied_cells_are_skipped() {
        let grid = metrics(1920, 1080, 72);
        let occupied = BTreeSet::from([Cell::new(0, 0), Cell::new(0, 2)]);

        let free = first_free_cells(grid, &occupied, 3);

        assert_eq!(
            free,
            vec![Cell::new(0, 1), Cell::new(0, 3), Cell::new(0, 4)]
        );
    }

    #[test]
    fn asking_for_nothing_returns_nothing() {
        let grid = metrics(1920, 1080, 72);

        assert!(first_free_cells(grid, &BTreeSet::new(), 0).is_empty());
    }

    /// More files than slots must still get a cell each — short-changing the
    /// caller would leave files with nowhere to go.
    #[test]
    fn a_full_grid_overflows_rather_than_returning_fewer_cells() {
        let grid = metrics(600, 400, 128);
        let capacity = (grid.columns * grid.rows) as usize;

        let free = first_free_cells(grid, &BTreeSet::new(), capacity + 5);

        assert_eq!(free.len(), capacity + 5);
        // Every cell distinct, and the overflow lies past the last column.
        assert_eq!(free.iter().collect::<BTreeSet<_>>().len(), capacity + 5);
        assert!(free[capacity].col >= grid.columns);
        // Clamping brings it back onto the last column, where it stacks visibly.
        assert!(grid.contains(grid.clamp_cell(free[capacity])));
    }

    #[test]
    fn a_stored_cell_from_a_larger_output_clamps_into_this_one() {
        let grid = metrics(1280, 720, 72);
        let off_screen = Cell::new(99, 99);

        let clamped = grid.clamp_cell(off_screen);

        assert!(grid.contains(clamped));
        assert_eq!(clamped, Cell::new(grid.columns - 1, grid.rows - 1));
    }
}
