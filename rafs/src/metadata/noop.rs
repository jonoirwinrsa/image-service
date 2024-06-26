// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! A noop meta data driver for place-holding.

use std::io::Result;
use std::sync::Arc;

use storage::device::{BlobChunkInfo, BlobDevice, BlobInfo};

use crate::metadata::{Inode, RafsInode, RafsSuperBlock, RafsSuperInodes};
use crate::{RafsInodeExt, RafsIoReader, RafsResult};

#[derive(Default)]
pub struct NoopSuperBlock {}

impl NoopSuperBlock {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RafsSuperInodes for NoopSuperBlock {
    fn get_max_ino(&self) -> Inode {
        unimplemented!()
    }

    fn get_inode(&self, _ino: Inode, _digest_validate: bool) -> Result<Arc<dyn RafsInode>> {
        unimplemented!()
    }

    fn get_extended_inode(
        &self,
        _ino: Inode,
        _validate_digest: bool,
    ) -> Result<Arc<dyn RafsInodeExt>> {
        unimplemented!()
    }
}

impl RafsSuperBlock for NoopSuperBlock {
    fn load(&mut self, _r: &mut RafsIoReader) -> Result<()> {
        unimplemented!()
    }

    fn update(&self, _r: &mut RafsIoReader) -> RafsResult<()> {
        unimplemented!()
    }

    fn destroy(&mut self) {}

    fn get_blob_infos(&self) -> Vec<Arc<BlobInfo>> {
        Vec::new()
    }

    fn root_ino(&self) -> u64 {
        unimplemented!()
    }

    fn get_chunk_info(&self, _idx: usize) -> Result<Arc<dyn BlobChunkInfo>> {
        unimplemented!("used by RAFS v6 only")
    }

    fn set_blob_device(&self, _blob_device: BlobDevice) {
        unimplemented!("used by RAFS v6 only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[should_panic]
    fn test_get_max_ino() {
        let blk = NoopSuperBlock::new();
        blk.get_max_ino();
    }

    #[test]
    #[should_panic]
    fn test_get_inode() {
        let blk = NoopSuperBlock::new();
        blk.get_inode(Inode::default(), false).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_get_extended_inode() {
        let blk = NoopSuperBlock::new();
        blk.get_extended_inode(Inode::default(), false).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_root_ino() {
        let blk = NoopSuperBlock::new();
        blk.root_ino();
    }

    #[test]
    #[should_panic]
    fn test_get_chunk_info() {
        let blk = NoopSuperBlock::new();
        blk.get_chunk_info(0).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_set_blob_device() {
        let blk = NoopSuperBlock::new();
        blk.set_blob_device(BlobDevice::default());
    }

    #[test]
    fn test_noop_super_block() {
        let mut blk = NoopSuperBlock::new();
        assert!(blk.get_blob_infos().is_empty());
        blk.destroy();
    }
}
