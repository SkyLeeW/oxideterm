use crate::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, DevicePixels, PlatformAtlas,
    Point, Size, TileId,
};
use anyhow::Result;
use collections::FxHashMap;
use parking_lot::Mutex;
use std::borrow::Cow;

pub(crate) struct SoftwareAtlas(Mutex<SoftwareAtlasState>);

struct SoftwareAtlasState {
    next_texture_id: u32,
    next_tile_id: u32,
    tiles_by_key: FxHashMap<AtlasKey, SoftwareAtlasEntry>,
    entries_by_texture: FxHashMap<AtlasTextureId, AtlasKey>,
}

struct SoftwareAtlasEntry {
    tile: AtlasTile,
    pixels: Vec<u8>,
}

impl SoftwareAtlas {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(SoftwareAtlasState {
            next_texture_id: 0,
            next_tile_id: 0,
            tiles_by_key: FxHashMap::default(),
            entries_by_texture: FxHashMap::default(),
        }))
    }

    pub(crate) fn with_pixels_for_tile<R>(
        &self,
        tile: &AtlasTile,
        read: impl FnOnce(AtlasTextureKind, Size<DevicePixels>, &[u8]) -> R,
    ) -> Option<R> {
        let lock = self.0.lock();
        let key = lock.entries_by_texture.get(&tile.texture_id)?;
        let entry = lock.tiles_by_key.get(key)?;
        Some(read(
            tile.texture_id.kind,
            entry.tile.bounds.size,
            &entry.pixels,
        ))
    }
}

impl PlatformAtlas for SoftwareAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut lock = self.0.lock();
        if let Some(entry) = lock.tiles_by_key.get(key) {
            return Ok(Some(entry.tile.clone()));
        }

        let Some((size, bytes)) = build()? else {
            return Ok(None);
        };

        let texture_id = AtlasTextureId {
            index: lock.next_texture_id,
            kind: key.texture_kind(),
        };
        lock.next_texture_id += 1;
        let tile = AtlasTile {
            texture_id,
            tile_id: TileId(lock.next_tile_id),
            padding: 0,
            bounds: Bounds {
                origin: Point::default(),
                size,
            },
        };
        lock.next_tile_id += 1;
        lock.entries_by_texture.insert(texture_id, key.clone());
        lock.tiles_by_key.insert(
            key.clone(),
            SoftwareAtlasEntry {
                tile: tile.clone(),
                pixels: bytes.into_owned(),
            },
        );
        Ok(Some(tile))
    }

    fn update(&self, key: &AtlasKey, bounds: Bounds<DevicePixels>, bytes: &[u8]) -> Result<()> {
        let mut lock = self.0.lock();
        let Some(entry) = lock.tiles_by_key.get_mut(key) else {
            return Ok(());
        };
        let bytes_per_pixel = match entry.tile.texture_id.kind {
            AtlasTextureKind::Monochrome => 1,
            AtlasTextureKind::Polychrome => 4,
        };
        let texture_width = entry.tile.bounds.size.width.0.max(0) as usize;
        let update_width = bounds.size.width.0.max(0) as usize;
        let update_height = bounds.size.height.0.max(0) as usize;
        let origin_x = bounds.origin.x.0.max(0) as usize;
        let origin_y = bounds.origin.y.0.max(0) as usize;

        for row in 0..update_height {
            let dst_start = ((origin_y + row) * texture_width + origin_x) * bytes_per_pixel;
            let src_start = row * update_width * bytes_per_pixel;
            let len = update_width * bytes_per_pixel;
            if dst_start + len <= entry.pixels.len() && src_start + len <= bytes.len() {
                entry.pixels[dst_start..dst_start + len]
                    .copy_from_slice(&bytes[src_start..src_start + len]);
            }
        }

        Ok(())
    }

    fn remove(&self, key: &AtlasKey) {
        let mut lock = self.0.lock();
        if let Some(entry) = lock.tiles_by_key.remove(key) {
            lock.entries_by_texture.remove(&entry.tile.texture_id);
        }
    }
}
