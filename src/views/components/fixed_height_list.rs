// SPDX-License-Identifier: GPL-3.0-only

//! A virtual list whose rows all have the same height.
//!
//! iced's virtual [`List`](cosmic::iced::widget::list::List) only renders
//! the rows in view, but it doesn't know how tall its rows are: whenever its
//! content is new, it builds and lays out **every** item (50 per frame) to
//! measure them, and draws nothing until it has. With a history of 5,910
//! tracks that is ~120 frames, about 3 s, before the list appears, and each
//! row built requests its cover, so opening the view fetched every cover in
//! the history.
//!
//! Track rows don't need measuring: [`build_track_row`](super::rows::build_track_row)
//! always shows one cover and two single-line texts, so every row is as tall
//! as the first. [`FixedHeightList`] measures that first row once and places
//! row `i` at `i * height`. It only ever builds the rows in the viewport, so
//! opening a list costs the same whatever its length, and covers load only
//! for rows that are actually shown.
//!
//! Rows of different heights would overlap; use iced's `List` for those.

use cosmic::iced::core::layout;
use cosmic::iced::core::mouse;
use cosmic::iced::core::overlay;
use cosmic::iced::core::renderer;
use cosmic::iced::core::widget::Operation;
use cosmic::iced::core::widget::tree::{self, Tree};
use cosmic::iced::core::{Clipboard, Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Vector, Widget};

/// A virtual list of equally tall rows. See the [module docs](self).
pub struct FixedHeightList<'a, T, Message, Theme, Renderer> {
    items: &'a [T],
    view_item: Box<dyn Fn(usize, &'a T) -> Element<'a, Message, Theme, Renderer> + 'a>,
    /// The rows in view, in the same order as [`State::rows`]. Rebuilt from
    /// `view_item` the first time this widget instance handles an event,
    /// because the app builds a new instance on every `view()`.
    visible: Vec<Element<'a, Message, Theme, Renderer>>,
}

impl<'a, T, Message, Theme, Renderer> FixedHeightList<'a, T, Message, Theme, Renderer> {
    /// A list of `items`, each built by `view_item(index, item)`.
    pub fn new(items: &'a [T], view_item: impl Fn(usize, &'a T) -> Element<'a, Message, Theme, Renderer> + 'a) -> Self {
        Self { items, view_item: Box::new(view_item), visible: Vec::new() }
    }
}

/// One row in view: its index, where it sits, and its widget state.
struct Row {
    index: usize,
    node: layout::Node,
    tree: Tree,
}

struct State {
    /// Which items the rows were built for: the slice's address and length.
    /// A different slice means different tracks, so the rows start over.
    items: (usize, usize),
    /// The (loosened) limits rows are laid out with.
    limits: layout::Limits,
    /// The size of the first row, measured with `limits`.
    row: Option<Size>,
    /// The rows in view, in index order.
    rows: Vec<Row>,
}

impl<'a, T, Message, Theme, Renderer> FixedHeightList<'a, T, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    /// The range of item indices that overlap `viewport`, for rows `height`
    /// tall starting at `top`.
    fn visible_range(&self, top: f32, height: f32, viewport: &Rectangle) -> std::ops::Range<usize> {
        if height <= 0.0 {
            return 0..0;
        }
        let len = self.items.len();
        let from = ((viewport.y - top) / height).floor().max(0.0) as usize;
        let to = ((viewport.y + viewport.height - top) / height).ceil().max(0.0) as usize;
        from.min(len)..to.min(len)
    }

    /// Make `self.visible` and `state.rows` hold exactly the rows in
    /// `viewport`, each built for this frame and laid out at its slot.
    ///
    /// A row that was already in view keeps its widget state (hover, press),
    /// reconciled against the freshly built row.
    fn sync_visible(&mut self, state: &mut State, layout: Layout<'_>, renderer: &Renderer, viewport: &Rectangle) {
        let Some(row) = state.row else {
            self.visible.clear();
            state.rows.clear();
            return;
        };
        let range = self.visible_range(layout.position().y, row.height, viewport);
        // The rows in view are contiguous, so their first index and count
        // describe them.
        let built = state.rows.first().map_or(0..0, |first| first.index..first.index + state.rows.len());
        if built == range && self.visible.len() == state.rows.len() {
            return;
        }

        let mut previous = std::mem::take(&mut state.rows);
        self.visible.clear();
        let items = self.items.get(range.clone()).unwrap_or_default();
        for (index, item) in range.zip(items) {
            let mut element = (self.view_item)(index, item);
            let mut tree = match previous.iter().position(|r| r.index == index) {
                Some(at) => {
                    let mut tree = previous.swap_remove(at).tree;
                    tree.diff(element.as_widget_mut());
                    tree
                }
                None => Tree::new(element.as_widget()),
            };
            let node =
                element.as_widget_mut().layout(&mut tree, renderer, &state.limits).move_to((0.0, index as f32 * row.height));
            state.rows.push(Row { index, node, tree });
            self.visible.push(element);
        }
    }
}

impl<'a, T, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for FixedHeightList<'a, T, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            items: (self.items.as_ptr() as usize, self.items.len()),
            limits: layout::Limits::NONE,
            row: None,
            rows: Vec::new(),
        })
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();

        let items = (self.items.as_ptr() as usize, self.items.len());
        if state.items != items {
            state.items = items;
            state.rows.clear();
            self.visible.clear();
        }

        // Measure the first row: once, and again whenever the width changes.
        let limits_now = limits.loose();
        if state.limits != limits_now || state.row.is_none() {
            state.limits = limits_now;
            state.rows.clear();
            self.visible.clear();
            state.row = self.items.first().map(|first| {
                let mut element = (self.view_item)(0, first);
                let mut tree = Tree::new(element.as_widget());
                element.as_widget_mut().layout(&mut tree, renderer, &state.limits).size()
            });
        }

        let row = state.row.unwrap_or(Size::ZERO);
        let intrinsic = Size::new(row.width, row.height * self.items.len() as f32);
        layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, intrinsic))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        self.sync_visible(state, layout, renderer, viewport);

        let offset = layout.position() - Point::ORIGIN;
        for (element, row) in self.visible.iter_mut().zip(&mut state.rows) {
            element.as_widget_mut().update(
                &mut row.tree,
                event,
                Layout::with_offset(offset, &row.node),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let offset = layout.position() - Point::ORIGIN;
        for (element, row) in self.visible.iter().zip(&state.rows) {
            element.as_widget().draw(&row.tree, renderer, theme, style, Layout::with_offset(offset, &row.node), cursor, viewport);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        let offset = layout.position() - Point::ORIGIN;
        self.visible
            .iter()
            .zip(&state.rows)
            .map(|(element, row)| {
                element.as_widget().mouse_interaction(
                    &row.tree,
                    Layout::with_offset(offset, &row.node),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        let state = tree.state.downcast_mut::<State>();
        let offset = layout.position() - Point::ORIGIN;
        for (element, row) in self.visible.iter_mut().zip(&mut state.rows) {
            element.as_widget_mut().operate(&mut row.tree, Layout::with_offset(offset, &row.node), renderer, operation);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let state = tree.state.downcast_mut::<State>();
        let offset = layout.position() - Point::ORIGIN;
        let children: Vec<_> = self
            .visible
            .iter_mut()
            .zip(&mut state.rows)
            .filter_map(|(element, row)| {
                element.as_widget_mut().overlay(
                    &mut row.tree,
                    Layout::with_offset(offset, &row.node),
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect();
        (!children.is_empty()).then(|| overlay::Group::with_children(children).overlay())
    }
}

impl<'a, T, Message, Theme, Renderer> From<FixedHeightList<'a, T, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    T: 'a,
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(list: FixedHeightList<'a, T, Message, Theme, Renderer>) -> Self {
        Element::new(list)
    }
}

#[cfg(test)]
mod tests {
    use super::FixedHeightList;
    use cosmic::iced::core::widget::{Tree, tree};
    use cosmic::iced::core::{
        Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Widget, clipboard, layout, mouse, renderer, window,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    const ROW: f32 = 50.0;
    const VIEW: f32 = 300.0;

    /// A row of known height whose tree state is its `marker`, recording
    /// where it was drawn. The null renderer runs it without a display.
    struct Probe {
        height: f32,
        marker: usize,
        draws: Rc<RefCell<Vec<(usize, Rectangle)>>>,
    }

    impl<Msg, Theme, Renderer: renderer::Renderer> Widget<Msg, Theme, Renderer> for Probe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fixed(self.height))
        }

        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<usize>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(self.marker)
        }

        fn layout(&mut self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
            layout::Node::new(Size::new(limits.max().width, self.height))
        }

        fn draw(
            &self,
            tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
            self.draws.borrow_mut().push((*tree.state.downcast_ref::<usize>(), layout.bounds()));
        }
    }

    type Headless<'a> = Element<'a, (), (), ()>;

    /// Records each row built (by index) and each row drawn (by the marker
    /// its *state* holds, and where).
    #[derive(Default)]
    struct Log {
        built: Rc<RefCell<Vec<usize>>>,
        draws: Rc<RefCell<Vec<(usize, Rectangle)>>>,
    }

    /// A list whose rows carry `marker_base + index` as their state.
    fn list<'a>(items: &'a [f32], marker_base: usize, log: &Log) -> Headless<'a> {
        let (built, draws) = (log.built.clone(), log.draws.clone());
        FixedHeightList::new(items, move |index, height| {
            built.borrow_mut().push(index);
            Headless::new(Probe { height: *height, marker: marker_base + index, draws: draws.clone() })
        })
        .into()
    }

    /// One frame scrolled to `scroll_y`: layout, a redraw event, then draw.
    /// Returns the list's laid-out height.
    fn frame(element: &mut Headless<'_>, tree: &mut Tree, scroll_y: f32, log: &Log) -> f32 {
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, f32::INFINITY));
        let node = element.as_widget_mut().layout(tree, &(), &limits);
        let viewport = Rectangle::new(Point::new(0.0, scroll_y), Size::new(300.0, VIEW));
        element.as_widget_mut().update(
            tree,
            &Event::Window(window::Event::RedrawRequested(std::time::Instant::now())),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard::Null,
            &mut Shell::new(&mut Vec::new()),
            &viewport,
        );
        log.draws.borrow_mut().clear();
        element.as_widget().draw(
            tree,
            &mut (),
            &(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        node.size().height
    }

    fn drawn_rows(log: &Log) -> Vec<f32> {
        log.draws.borrow().iter().map(|(_, bounds)| bounds.y).collect()
    }

    #[test]
    fn a_long_list_builds_only_the_first_row_and_the_rows_in_view() {
        let items = vec![ROW; 5_000];
        let log = Log::default();
        let mut element = list(&items, 0, &log);
        let mut tree = Tree::new(element.as_widget());

        let height = frame(&mut element, &mut tree, 0.0, &log);

        assert_eq!(height, ROW * 5_000.0, "the list reserves the space of every row");
        // One build to measure row 0, then the six rows in a 300px viewport.
        assert_eq!(*log.built.borrow(), [0, 0, 1, 2, 3, 4, 5]);
        assert_eq!(drawn_rows(&log), [0.0, 50.0, 100.0, 150.0, 200.0, 250.0]);
    }

    #[test]
    fn scrolling_builds_and_places_only_the_rows_now_in_view() {
        let items = vec![ROW; 5_000];
        let log = Log::default();
        let mut element = list(&items, 0, &log);
        let mut tree = Tree::new(element.as_widget());
        frame(&mut element, &mut tree, 0.0, &log);
        log.built.borrow_mut().clear();

        // Halfway through row 4000: rows 4000..=4006 overlap the viewport.
        frame(&mut element, &mut tree, 4_000.5 * ROW, &log);

        assert_eq!(*log.built.borrow(), (4_000..=4_006).collect::<Vec<_>>());
        let expected: Vec<f32> = (4_000..=4_006).map(|i| i as f32 * ROW).collect();
        assert_eq!(drawn_rows(&log), expected);
    }

    #[test]
    fn rows_keep_their_state_while_in_view_and_reset_for_a_new_list() {
        let first = vec![ROW; 100];
        let log = Log::default();
        let mut element = list(&first, 0, &log);
        let mut tree = Tree::new(element.as_widget());
        frame(&mut element, &mut tree, 0.0, &log);

        // The app builds a new widget every view(); rows still in view keep
        // the state they were created with (marker 0..), not the new rows'.
        let mut rebuilt = list(&first, 1_000, &log);
        tree.diff(rebuilt.as_widget_mut());
        frame(&mut rebuilt, &mut tree, ROW, &log);
        let markers: Vec<usize> = log.draws.borrow().iter().map(|(marker, _)| *marker).collect();
        assert_eq!(markers, [1, 2, 3, 4, 5, 1_006], "rows 1..=5 kept their state; row 6 is new");

        // A different list (other tracks) must not inherit those rows.
        let second = vec![ROW; 100];
        let mut other = list(&second, 2_000, &log);
        tree.diff(other.as_widget_mut());
        frame(&mut other, &mut tree, ROW, &log);
        let markers: Vec<usize> = log.draws.borrow().iter().map(|(marker, _)| *marker).collect();
        assert_eq!(markers, (2_001..=2_006).collect::<Vec<_>>());
    }

    #[test]
    fn an_empty_list_has_no_height_and_builds_nothing() {
        let items: Vec<f32> = Vec::new();
        let log = Log::default();
        let mut element = list(&items, 0, &log);
        let mut tree = Tree::new(element.as_widget());

        assert_eq!(frame(&mut element, &mut tree, 0.0, &log), 0.0);
        assert!(log.built.borrow().is_empty());
        assert!(log.draws.borrow().is_empty());
    }
}
