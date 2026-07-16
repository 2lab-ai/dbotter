use std::ops::RangeInclusive;

/// Responsive dimensions and stable region registries for the native workspace.
pub struct NativeLayout;

impl NativeLayout {
    // Retained aliases used by the delivered shell while it adopts the D11 model.
    pub const CONNECTIONS_WIDTH: f32 = 264.0;
    pub const EXPLORER_WIDTH: f32 = 312.0;
    pub const COLLAPSE_WIDTH: f32 = 840.0;

    pub const WIDE_MIN_WIDTH: f32 = 1180.0;
    pub const WIDE_MIN_HEIGHT: f32 = 720.0;
    pub const NAVIGATOR_DEFAULT_WIDTH: f32 = 280.0;
    pub const NAVIGATOR_WIDTH_RANGE: RangeInclusive<f32> = 220.0..=420.0;
    pub const CENTER_MIN_WIDTH: f32 = 520.0;
    pub const SUBORDINATE_MIN_EXTENT: f32 = 240.0;
    pub const PANE_MIN_EXTENT: f32 = 160.0;
    pub const DEFAULT_EDITOR_SHARE: f32 = 0.60;
    pub const SPLITTER_KEYBOARD_STEP: f32 = 5.0;
    pub const SPLITTER_ACCESSIBLE_HIT_EXTENT: f32 = 44.0;
    pub const ACTION_MIN_SIZE: [f32; 2] = [44.0, 44.0];
    pub const ADJACENT_ACTION_GAP: f32 = 8.0;
    pub const DENSE_ROW_HEIGHT: f32 = 30.0;

    pub const P0_REGION_IDS: [&'static str; 4] = [
        "navigator",
        "object-editor-tabs",
        "result-history-tabs",
        "status-action-context",
    ];

    pub const P0_WORKSPACE_VIEW_IDS: [&'static str; 8] = [
        "data",
        "structure",
        "new-editor",
        "grid",
        "record",
        "history",
        "review",
        "redis-value",
    ];

    pub const fn columns_for_width(width: f32) -> usize {
        if width < Self::COLLAPSE_WIDTH { 1 } else { 3 }
    }

    pub fn resolve(width: f32, height: f32, geometry: WorkspaceGeometry) -> ResolvedLayout {
        let is_wide = width.is_finite()
            && height.is_finite()
            && width >= Self::WIDE_MIN_WIDTH
            && height >= Self::WIDE_MIN_HEIGHT;

        if is_wide {
            let navigator_width = geometry.navigator_width();
            let center_width = (width - navigator_width).max(0.0);
            let maximum_subordinate =
                (height - Self::PANE_MIN_EXTENT).max(Self::SUBORDINATE_MIN_EXTENT);
            let requested_subordinate = height * (1.0 - geometry.editor_share());
            let subordinate_extent = requested_subordinate
                .max(Self::SUBORDINATE_MIN_EXTENT)
                .min(maximum_subordinate);
            ResolvedLayout {
                mode: LayoutMode::Wide,
                navigator_width: Some(navigator_width),
                center_width,
                subordinate_extent,
            }
        } else {
            let center_width = if width.is_finite() {
                width.max(0.0)
            } else {
                0.0
            };
            let subordinate_extent = if height.is_finite() {
                height.max(0.0)
            } else {
                0.0
            };
            ResolvedLayout {
                mode: LayoutMode::Compact,
                navigator_width: None,
                center_width,
                subordinate_extent,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Compact,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkspaceGeometry {
    navigator_width: f32,
    editor_share: f32,
    inspector_visible: bool,
}

impl WorkspaceGeometry {
    pub fn restore(navigator_width: f32, editor_share: f32, inspector_visible: bool) -> Self {
        let navigator_is_valid = navigator_width.is_finite()
            && navigator_width >= *NativeLayout::NAVIGATOR_WIDTH_RANGE.start()
            && navigator_width <= *NativeLayout::NAVIGATOR_WIDTH_RANGE.end();
        let split_is_valid = editor_share.is_finite() && editor_share > 0.0 && editor_share < 1.0;
        if !navigator_is_valid || !split_is_valid {
            return Self::default();
        }
        Self {
            navigator_width,
            editor_share,
            inspector_visible,
        }
    }

    pub const fn navigator_width(self) -> f32 {
        self.navigator_width
    }

    pub const fn editor_share(self) -> f32 {
        self.editor_share
    }

    pub const fn inspector_visible(self) -> bool {
        self.inspector_visible
    }
}

impl Default for WorkspaceGeometry {
    fn default() -> Self {
        Self {
            navigator_width: NativeLayout::NAVIGATOR_DEFAULT_WIDTH,
            editor_share: NativeLayout::DEFAULT_EDITOR_SHARE,
            inspector_visible: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedLayout {
    mode: LayoutMode,
    navigator_width: Option<f32>,
    center_width: f32,
    subordinate_extent: f32,
}

impl ResolvedLayout {
    pub const fn mode(self) -> LayoutMode {
        self.mode
    }

    pub const fn navigator_is_persistent(self) -> bool {
        matches!(self.mode, LayoutMode::Wide)
    }

    pub const fn navigator_width(self) -> Option<f32> {
        self.navigator_width
    }

    pub const fn center_width(self) -> f32 {
        self.center_width
    }

    pub const fn subordinate_extent(self) -> f32 {
        self.subordinate_extent
    }

    pub const fn visible_region_ids(self) -> [&'static str; 4] {
        NativeLayout::P0_REGION_IDS
    }

    pub const fn status_action_context_visible(self) -> bool {
        true
    }

    pub const fn uses_horizontal_overflow(self) -> bool {
        false
    }

    pub const fn uses_named_navigator_drawer(self) -> bool {
        matches!(self.mode, LayoutMode::Compact)
    }

    pub const fn uses_one_at_a_time_inspector(self) -> bool {
        matches!(self.mode, LayoutMode::Compact)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Editor,
    Subordinate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitLayout {
    total_extent: f32,
    editor_extent: Option<f32>,
    subordinate_extent: Option<f32>,
    collapsed: Option<Pane>,
}

impl SplitLayout {
    pub fn from_editor_extent(total_extent: f32, editor_extent: f32) -> Self {
        let total_extent = normalize_total_extent(total_extent);
        if !editor_extent.is_finite() {
            return Self::reset(total_extent);
        }
        if editor_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Editor);
        }
        let subordinate_extent = total_extent - editor_extent;
        if subordinate_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Subordinate);
        }
        Self {
            total_extent,
            editor_extent: Some(editor_extent),
            subordinate_extent: Some(subordinate_extent),
            collapsed: None,
        }
    }

    pub fn from_subordinate_extent(total_extent: f32, subordinate_extent: f32) -> Self {
        let total_extent = normalize_total_extent(total_extent);
        if !subordinate_extent.is_finite() {
            return Self::reset(total_extent);
        }
        if subordinate_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Subordinate);
        }
        let editor_extent = total_extent - subordinate_extent;
        if editor_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Editor);
        }
        Self {
            total_extent,
            editor_extent: Some(editor_extent),
            subordinate_extent: Some(subordinate_extent),
            collapsed: None,
        }
    }

    pub fn reset(total_extent: f32) -> Self {
        let total_extent = normalize_total_extent(total_extent);
        let editor_extent = (f64::from(total_extent) * 0.60_f64) as f32;
        let subordinate_extent = total_extent - editor_extent;
        if editor_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Editor);
        }
        if subordinate_extent < NativeLayout::PANE_MIN_EXTENT {
            return Self::collapsed(total_extent, Pane::Subordinate);
        }
        Self {
            total_extent,
            editor_extent: Some(editor_extent),
            subordinate_extent: Some(subordinate_extent),
            collapsed: None,
        }
    }

    pub const fn editor_extent(self) -> Option<f32> {
        self.editor_extent
    }

    pub const fn subordinate_extent(self) -> Option<f32> {
        self.subordinate_extent
    }

    pub const fn editor_restore_label(self) -> Option<&'static str> {
        if matches!(self.collapsed, Some(Pane::Editor)) {
            Some("Restore editor")
        } else {
            None
        }
    }

    pub const fn subordinate_restore_label(self) -> Option<&'static str> {
        if matches!(self.collapsed, Some(Pane::Subordinate)) {
            Some("Restore results/history")
        } else {
            None
        }
    }

    pub fn restore(&mut self, pane: Pane) {
        if self.collapsed == Some(pane) {
            self.reset_to_default();
        }
    }

    pub fn keyboard_adjust(&mut self, steps: i32) {
        let (Some(editor_extent), Some(_)) = (self.editor_extent, self.subordinate_extent) else {
            return;
        };
        let adjusted = editor_extent + steps as f32 * NativeLayout::SPLITTER_KEYBOARD_STEP;
        *self = Self::from_editor_extent(self.total_extent, adjusted);
    }

    pub fn reset_to_default(&mut self) {
        *self = Self::reset(self.total_extent);
    }

    fn collapsed(total_extent: f32, pane: Pane) -> Self {
        match pane {
            Pane::Editor => Self {
                total_extent,
                editor_extent: None,
                subordinate_extent: Some(total_extent),
                collapsed: Some(Pane::Editor),
            },
            Pane::Subordinate => Self {
                total_extent,
                editor_extent: Some(total_extent),
                subordinate_extent: None,
                collapsed: Some(Pane::Subordinate),
            },
        }
    }
}

fn normalize_total_extent(total_extent: f32) -> f32 {
    if total_extent.is_finite() && total_extent >= NativeLayout::PANE_MIN_EXTENT {
        total_extent
    } else {
        NativeLayout::PANE_MIN_EXTENT / NativeLayout::DEFAULT_EDITOR_SHARE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackSurface {
    Navigator,
    Inspector,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactFallback {
    visible_surface: Option<FallbackSurface>,
    restore_focus_id: Option<String>,
}

impl CompactFallback {
    pub fn open(&mut self, surface: FallbackSurface, restore_focus_id: &str) {
        self.visible_surface = Some(surface);
        self.restore_focus_id = Some(restore_focus_id.to_owned());
    }

    pub const fn visible_surface(&self) -> Option<FallbackSurface> {
        self.visible_surface
    }

    pub fn restore_focus_id(&self) -> Option<&str> {
        self.restore_focus_id.as_deref()
    }

    pub const fn covers_status_action_context(&self) -> bool {
        false
    }

    pub fn close(&mut self) -> Option<String> {
        self.visible_surface = None;
        self.restore_focus_id.take()
    }
}
