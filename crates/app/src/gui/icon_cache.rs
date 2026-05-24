//! Decode + cache item icons as egui textures.

use crate::assets;
use std::collections::HashMap;

pub struct IconCache {
    handles: HashMap<String, Option<egui::TextureHandle>>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconCache {
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
        }
    }

    /// Get (or lazily load) the texture for an icon. Returns `None` if the
    /// asset is missing or undecodable; callers should fall back to a
    /// placeholder rect.
    pub fn get(&mut self, ctx: &egui::Context, icon_path: &str) -> Option<&egui::TextureHandle> {
        if !self.handles.contains_key(icon_path) {
            let handle = decode_to_texture(ctx, icon_path);
            self.handles.insert(icon_path.to_string(), handle);
        }
        self.handles.get(icon_path).and_then(|h| h.as_ref())
    }
}

fn decode_to_texture(ctx: &egui::Context, icon_path: &str) -> Option<egui::TextureHandle> {
    let bytes = assets::read_icon(icon_path)?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    let handle = ctx.load_texture(icon_path, color, egui::TextureOptions::LINEAR);
    Some(handle)
}
