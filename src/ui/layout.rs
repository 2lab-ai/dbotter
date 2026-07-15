/// Responsive dimensions for the native database workspace.
pub struct NativeLayout;

impl NativeLayout {
    pub const CONNECTIONS_WIDTH: f32 = 264.0;
    pub const EXPLORER_WIDTH: f32 = 312.0;
    pub const COLLAPSE_WIDTH: f32 = 840.0;

    pub const fn columns_for_width(width: f32) -> usize {
        if width < Self::COLLAPSE_WIDTH { 1 } else { 3 }
    }
}
