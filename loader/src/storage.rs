use atelier_core::{AssetRef, AssetTypeId, AssetUuid};
use crossbeam_channel::Sender;
use dashmap::DashMap;
use std::{
    error::Error,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Loading ID allocated by `atelier-assets` to track loading of a particular asset.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub struct LoadHandle(pub u64);

impl LoadHandle {
    /// Returns true if the handle needs to be resolved through the [`IndirectionTable`] before use.
    /// An "indirect" LoadHandle represents a load operation for an identifier that is late-bound,
    /// meaning the identifier may change which [`AssetUuid`] it resolves to.
    /// An example of an indirect LoadHandle would be one that loads by filesystem path.
    /// The specific asset at a path may change as files change, move or are deleted, while a direct
    /// LoadHandle (one that addresses by AssetUuid) is guaranteed to refer to an AssetUuid for its
    /// whole lifetime.
    pub fn is_indirect(&self) -> bool {
        (self.0 & (1 << 63)) == 1 << 63
    }
}

pub(crate) enum HandleOp {
    Error(LoadHandle, u32, Box<dyn Error + Send>),
    Complete(LoadHandle, u32),
    Drop(LoadHandle, u32),
}

/// Type that allows the downstream asset storage implementation to signal that this asset is
/// loaded.
pub struct AssetLoadOp {
    sender: Option<Sender<HandleOp>>,
    handle: LoadHandle,
    version: u32,
}

impl AssetLoadOp {
    pub(crate) fn new(sender: Sender<HandleOp>, handle: LoadHandle, version: u32) -> Self {
        Self {
            sender: Some(sender),
            handle,
            version,
        }
    }

    /// Returns the `LoadHandle` associated with the load operation
    pub fn load_handle(&self) -> LoadHandle {
        self.handle
    }

    /// Signals that this load operation has completed succesfully.
    pub fn complete(mut self) {
        let _ = self
            .sender
            .as_ref()
            .unwrap()
            .send(HandleOp::Complete(self.handle, self.version));
        self.sender = None;
    }

    /// Signals that this load operation has completed with an error.
    pub fn error<E: Error + 'static + Send>(mut self, error: E) {
        let _ = self.sender.as_ref().unwrap().send(HandleOp::Error(
            self.handle,
            self.version,
            Box::new(error),
        ));
        self.sender = None;
    }
}

impl Drop for AssetLoadOp {
    fn drop(&mut self) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(HandleOp::Drop(self.handle, self.version));
        }
    }
}

/// Storage for all assets of all asset types.
///
/// Consumers are expected to provide the implementation for this, as this is the bridge between
/// `atelier-assets` and the application.
pub trait AssetStorage {
    /// Updates the backing data of an asset.
    ///
    /// An example usage of this is when a texture such as "player.png" changes while the
    /// application is running. The asset ID is the same, but the underlying pixel data can differ.
    ///
    /// # Parameters
    ///
    /// * `loader`: Loader implementation calling this function.
    /// * `asset_type_id`: UUID of the asset type.
    /// * `data`: The updated asset byte data.
    /// * `load_handle`: ID allocated by `atelier-assets` to track loading of a particular asset.
    /// * `load_op`: Allows the loading implementation to signal when loading is done / errors.
    /// * `version`: Runtime load version of this asset, increments each time the asset is updated.
    fn update_asset(
        &self,
        loader_info: &dyn LoaderInfoProvider,
        asset_type_id: &AssetTypeId,
        data: &[u8],
        load_handle: LoadHandle,
        load_op: AssetLoadOp,
        version: u32,
    ) -> Result<(), Box<dyn Error>>;

    /// Commits the specified asset version as loaded and ready to use.
    ///
    /// # Parameters
    ///
    /// * `asset_type_id`: UUID of the asset type.
    /// * `load_handle`: ID allocated by `atelier-assets` to track loading of a particular asset.
    /// * `version`: Runtime load version of this asset, increments each time the asset is updated.
    fn commit_asset_version(&self, asset_type: &AssetTypeId, load_handle: LoadHandle, version: u32);

    /// Frees the asset identified by the load handle.
    ///
    /// # Parameters
    ///
    /// * `asset_type_id`: UUID of the asset type.
    /// * `load_handle`: ID allocated by `atelier-assets` to track loading of a particular asset.
    fn free(&self, asset_type_id: &AssetTypeId, load_handle: LoadHandle);
}

/// Asset loading status.
#[derive(Debug)]
pub enum LoadStatus {
    /// There is no request for the asset to be loaded.
    NotRequested,
    /// The asset is being loaded.
    Loading,
    /// The asset is loaded.
    Loaded,
    /// The asset is being unloaded.
    Unloading,
    /// The asset does not exist.
    DoesNotExist,
    /// There was an error during loading / unloading of the asset.
    Error(Box<dyn Error>),
}

/// Indicates the number of references there are to an asset.
///
/// **Note:** The information is true at the time the `LoadInfo` is retrieved. The actual number of
/// references may change.
pub struct LoadInfo {
    /// UUID of the asset.
    pub asset_id: AssetUuid,
    /// Number of references to the asset.
    pub refs: u32,
}

/// Provides information about mappings between `AssetUuid` and `LoadHandle`.
/// Intended to be used for `Handle` serde.
pub trait LoaderInfoProvider: Send + Sync {
    /// Returns the load handle for the asset with the given UUID, if present.
    ///
    /// This will only return `Some(..)` if there has been a previous call to [`Loader::add_ref`].
    ///
    /// # Parameters
    ///
    /// * `id`: UUID of the asset.
    fn get_load_handle(&self, asset_ref: &AssetRef) -> Option<LoadHandle>;

    /// Returns the AssetUUID for the given LoadHandle, if present.
    ///
    /// # Parameters
    ///
    /// * `load_handle`: ID allocated by `atelier-assets` to track loading of the asset.
    fn get_asset_id(&self, load: LoadHandle) -> Option<AssetUuid>;
}

/// Allocates LoadHandles for [`Loader`] implementations.
pub trait HandleAllocator: Send + Sync + 'static {
    /// Allocates a [`LoadHandle`] for use by a [`Loader`].
    /// The same LoadHandle must not be returned by this function until it has been passed to `free`.
    /// NOTE: The most significant bit of the u64 in the LoadHandle returned MUST be unset,
    /// as it is reserved for indicating whether the handle is indirect or not.
    fn alloc(&self) -> LoadHandle;
    /// Frees a [`LoadHandle`], allowing the handle to be returned by a future `alloc` call.
    fn free(&self, handle: LoadHandle);
}

/// An implementation of [`HandleAllocator`] which uses an incrementing AtomicU64 internally to allocate LoadHandle IDs.
pub struct AtomicHandleAllocator(AtomicU64);
impl AtomicHandleAllocator {
    pub const fn new(starting_value: u64) -> Self {
        Self(AtomicU64::new(starting_value))
    }
}
impl Default for AtomicHandleAllocator {
    fn default() -> Self {
        Self(AtomicU64::new(1))
    }
}
impl HandleAllocator for AtomicHandleAllocator {
    fn alloc(&self) -> LoadHandle {
        LoadHandle(self.0.fetch_add(1, Ordering::Relaxed))
    }
    fn free(&self, _handle: LoadHandle) {}
}

impl HandleAllocator for &'static AtomicHandleAllocator {
    fn alloc(&self) -> LoadHandle {
        LoadHandle(self.0.fetch_add(1, Ordering::Relaxed))
    }
    fn free(&self, _handle: LoadHandle) {}
}

trait IndirectionResolver {}

/// Resolves indirect [`LoadHandle`]s. See [`LoadHandle::is_indirect`] for details.
#[derive(Clone)]
pub struct IndirectionTable(pub(crate) Arc<DashMap<LoadHandle, LoadHandle>>);