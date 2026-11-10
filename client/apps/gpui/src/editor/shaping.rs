use super::*;

/// Everything a cached shaped line depends on besides the line text itself.
/// A probe mismatch (font change, zoom, tab width, theme/family/mode switch —
/// captured by the full set of colors `syntax_runs` paints with) empties the
/// cache wholesale.
#[derive(PartialEq, Clone)]
pub(super) struct ShapeProbe {
    pub(super) font: Font,
    pub(super) font_size: Pixels,
    pub(super) tab_width: usize,
    /// The dialect the classifier runs under. Part of the probe because the
    /// same bytes highlight differently per engine (`#` opens a comment in
    /// MySQL and names a temp table in T-SQL), so a connection switch must
    /// drop every shaped line.
    pub(super) dialect: sqlly_lang::sqlmodel::Dialect,
    pub(super) colors: [Hsla; 11],
    /// Which word kinds draw bold.
    pub(super) bold: crate::preferences::SyntaxBoldPreferences,
    /// Address of the data-model name set bold resolves against, so a new
    /// catalog reshapes every line.
    pub(super) names: usize,
}

impl ShapeProbe {
    pub(super) fn current(
        font: &Font,
        font_size: Pixels,
        tab_width: usize,
        dialect: sqlly_lang::sqlmodel::Dialect,
    ) -> Self {
        Self {
            font: font.clone(),
            font_size,
            tab_width,
            dialect,
            colors: [
                Theme::text(),
                Theme::text_dim(),
                Theme::keyword(),
                Theme::string(),
                Theme::comment(),
                Theme::number(),
                Theme::kind_column(),
                Theme::function(),
                Theme::constant(),
                Theme::type_name(),
                Theme::variable(),
            ],
            bold: crate::preferences::SyntaxBoldPreferences::default(),
            names: 0,
        }
    }

    pub(super) fn with_bold(
        mut self,
        bold: crate::preferences::SyntaxBoldPreferences,
        names: &std::sync::Arc<crate::sql_syntax::SemanticNames>,
    ) -> Self {
        self.bold = bold;
        self.names = std::sync::Arc::as_ptr(names) as usize;
        self
    }
}

#[derive(Default)]
pub(super) struct ShapeCache {
    pub(super) probe: Option<ShapeProbe>,
    /// Per-line shaped text. Keys may carry a `\u{0}`-separated inlay signature after the line text.
    pub(super) map: std::collections::HashMap<String, (crate::tab_map::TabMap, ShapedLine)>,
    /// Shaped gutter labels (`"1"`, `"2"`, …). Same probe as `map`, so a
    /// font/size/theme change drops both.
    pub(super) gutter: std::collections::HashMap<String, ShapedLine>,
}

impl ShapeCache {
    /// Clear the cache when any shaping input other than line text changed.
    pub(super) fn sync_probe(&mut self, probe: &ShapeProbe) {
        if self.probe.as_ref() != Some(probe) {
            self.map.clear();
            self.gutter.clear();
            self.probe = Some(probe.clone());
        }
    }

    pub(super) fn get(&self, line: &str) -> Option<(crate::tab_map::TabMap, ShapedLine)> {
        self.map.get(line).cloned()
    }

    pub(super) fn insert(&mut self, line: &str, value: (crate::tab_map::TabMap, ShapedLine)) {
        // Backstop against unbounded growth on huge documents; a clear just
        // re-shapes the next frame's visible lines.
        if self.map.len() >= 4096 {
            self.map.clear();
        }
        self.map.insert(line.to_string(), value);
    }

    pub(super) fn get_gutter(&self, label: &str) -> Option<ShapedLine> {
        self.gutter.get(label).cloned()
    }

    pub(super) fn insert_gutter(&mut self, label: &str, shaped: ShapedLine) {
        if self.gutter.len() >= 4096 {
            self.gutter.clear();
        }
        self.gutter.insert(label.to_string(), shaped);
    }
}
