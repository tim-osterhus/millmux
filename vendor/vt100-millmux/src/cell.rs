/// Represents a single terminal cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    contents: String,
    occupied: bool,
    wide: bool,
    wide_continuation: bool,
    attrs: crate::attrs::Attrs,
}

impl Cell {
    pub(crate) fn new() -> Self {
        Self {
            contents: String::new(),
            occupied: false,
            wide: false,
            wide_continuation: false,
            attrs: crate::attrs::Attrs::default(),
        }
    }

    pub(crate) fn set_grapheme(&mut self, grapheme: &str, wide: bool, attrs: crate::attrs::Attrs) {
        self.contents.clear();
        self.contents.push_str(grapheme);
        self.occupied = true;
        self.wide = wide;
        self.wide_continuation = false;
        self.attrs = attrs;
    }

    pub(crate) fn clear(&mut self, attrs: crate::attrs::Attrs) {
        self.contents.clear();
        self.occupied = false;
        self.wide = false;
        self.wide_continuation = false;
        self.attrs = attrs;
    }

    /// Returns the text contents of the cell.
    ///
    /// Can include multiple unicode characters if combining characters are
    /// used, but will contain at most one character with a non-zero character
    /// width.
    #[must_use]
    pub fn contents(&self) -> &str {
        &self.contents
    }

    /// Returns whether the cell contains any text data.
    #[must_use]
    pub fn has_contents(&self) -> bool {
        !self.contents.is_empty()
    }

    /// Returns whether output has intentionally occupied this cell.
    #[must_use]
    pub fn is_occupied(&self) -> bool {
        self.occupied
    }

    pub(crate) fn mark_occupied_blank(&mut self) {
        self.occupied = true;
    }

    /// Returns whether the text data in the cell represents a wide character.
    #[must_use]
    pub fn is_wide(&self) -> bool {
        self.wide
    }

    /// Returns whether the cell contains the second half of a wide character
    /// (in other words, whether the previous cell in the row contains a wide
    /// character)
    #[must_use]
    pub fn is_wide_continuation(&self) -> bool {
        self.wide_continuation
    }

    pub(crate) fn set_wide_continuation(&mut self, wide: bool) {
        self.wide_continuation = wide;
    }

    pub(crate) fn attrs(&self) -> &crate::attrs::Attrs {
        &self.attrs
    }

    /// Returns the foreground color of the cell.
    #[must_use]
    pub fn fgcolor(&self) -> crate::Color {
        self.attrs.fgcolor
    }

    /// Returns the background color of the cell.
    #[must_use]
    pub fn bgcolor(&self) -> crate::Color {
        self.attrs.bgcolor
    }

    /// Returns whether the cell should be rendered with the bold text
    /// attribute.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.attrs.bold()
    }

    /// Returns whether the cell should be rendered with the dim text
    /// attribute.
    #[must_use]
    pub fn dim(&self) -> bool {
        self.attrs.dim()
    }

    /// Returns whether the cell should be rendered with the italic text
    /// attribute.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.attrs.italic()
    }

    /// Returns whether the cell should be rendered with the underlined text
    /// attribute.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.attrs.underline()
    }

    /// Returns whether the cell should be rendered with the inverse text
    /// attribute.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.attrs.inverse()
    }
}
